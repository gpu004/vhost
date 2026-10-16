use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use hegel::generators as gs;
use hegel::TestCase;
use nix::sys::memfd::{memfd_create, MFdFlags};
use vhost::vhost_user::message::{VhostUserHeaderFlag, VhostUserProtocolFeatures};
use vhost::vhost_user::{Frontend, VhostUserFrontend};
use vhost::{VhostBackend, VhostUserDirtyLogRegion, VhostUserMemoryRegionInfo};
use vhost_user_backend::bitmap::BitmapMmapRegion;
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock};
use vm_memory::{Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryMmap};
use vmm_sys_util::epoll::EventSet;
use vmm_sys_util::event::{new_event_consumer_and_notifier, EventFlag};

const REGION_SIZE: usize = 4096;
const FIRST_GPA: u64 = 0x10_0000;

type LoggedMemory = GuestMemoryAtomic<GuestMemoryMmap<BitmapMmapRegion>>;
type LoggedVring = VringRwLock<LoggedMemory>;

struct DirtyLogBackend {
    memory: Arc<Mutex<Option<LoggedMemory>>>,
}

impl VhostUserBackendMut for DirtyLogBackend {
    type Bitmap = BitmapMmapRegion;
    type Vring = LoggedVring;

    fn num_queues(&self) -> usize {
        1
    }

    fn max_queue_size(&self) -> usize {
        256
    }

    fn features(&self) -> u64 {
        vhost::vhost_user::VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::LOG_SHMFD
            | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
            | VhostUserProtocolFeatures::REPLY_ACK
    }

    fn set_event_idx(&mut self, _enabled: bool) {}

    fn exit_event(
        &self,
        _thread_index: usize,
    ) -> Option<(
        vmm_sys_util::event::EventConsumer,
        vmm_sys_util::event::EventNotifier,
    )> {
        Some(new_event_consumer_and_notifier(EventFlag::empty()).unwrap())
    }

    fn update_memory(&mut self, memory: LoggedMemory) -> std::io::Result<()> {
        *self.memory.lock().unwrap() = Some(memory);
        Ok(())
    }

    fn handle_event(
        &mut self,
        _device_event: u16,
        _evset: EventSet,
        _vrings: &[LoggedVring],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

struct Region {
    file: File,
    guest_addr: u64,
}

impl Region {
    fn new(index: usize) -> Self {
        let file = File::from(memfd_create("hegel-memory", MFdFlags::empty()).unwrap());
        file.set_len(REGION_SIZE as u64).unwrap();
        Region {
            file,
            guest_addr: FIRST_GPA + index as u64 * 0x10_0000,
        }
    }

    fn info(&self) -> VhostUserMemoryRegionInfo {
        VhostUserMemoryRegionInfo::new(
            self.guest_addr,
            REGION_SIZE as u64,
            self.guest_addr,
            0,
            self.file.as_raw_fd(),
        )
    }
}

enum Operation {
    Add,
    Replace { regions: Vec<usize> },
    Write { region: usize, offset: usize },
}

fn draw_operations(tc: TestCase) -> Vec<Operation> {
    let operation_count = tc.draw(gs::integers::<usize>().min_value(1).max_value(12));
    let mut region_count = 1;
    let mut active_regions = vec![0];
    let mut operations = Vec::with_capacity(operation_count);
    for _ in 0..operation_count {
        match tc.draw(gs::integers::<u8>().min_value(0).max_value(2)) {
            0 => {
                operations.push(Operation::Add);
                active_regions.push(region_count);
                region_count += 1;
            }
            1 => {
                let mask = tc.draw(
                    gs::integers::<usize>()
                        .min_value(1)
                        .max_value((1 << region_count) - 1),
                );
                active_regions = (0..region_count)
                    .filter(|index| mask & (1 << index) != 0)
                    .collect();
                operations.push(Operation::Replace {
                    regions: active_regions.clone(),
                });
            }
            _ => {
                let selected = tc.draw(
                    gs::integers::<usize>()
                        .min_value(0)
                        .max_value(active_regions.len() - 1),
                );
                operations.push(Operation::Write {
                    region: active_regions[selected],
                    offset: tc.draw(
                        gs::integers::<usize>()
                            .min_value(0)
                            .max_value(REGION_SIZE - 1),
                    ),
                });
            }
        }
    }
    operations
}

fn create_memfd(name: &str, len: u64) -> File {
    let file = File::from(memfd_create(name, MFdFlags::empty()).unwrap());
    file.set_len(len).unwrap();
    file
}

fn run_dirty_log_sequence(path: PathBuf, operations: Vec<Operation>) {
    let memory = Arc::new(Mutex::new(None));
    let guest_memory = GuestMemoryAtomic::new(GuestMemoryMmap::<BitmapMmapRegion>::new());
    let backend = Arc::new(Mutex::new(DirtyLogBackend {
        memory: memory.clone(),
    }));
    let daemon = VhostUserDaemon::new("hegel-dirty-log".to_owned(), backend, guest_memory).unwrap();
    let initial_region = Region::new(0);
    let log_file = create_memfd("hegel-log", 4096);
    let log_fd = log_file.as_raw_fd();
    let client_path = path.clone();
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let mut daemon = daemon;
        daemon.serve(server_path).unwrap();
    });
    while !path.exists() {
        thread::yield_now();
    }
    let client = thread::spawn(move || {
        let mut frontend = Frontend::connect(client_path, 1).unwrap();
        frontend.set_hdr_flags(VhostUserHeaderFlag::NEED_REPLY);
        frontend.set_owner().unwrap();
        let features = frontend.get_features().unwrap();
        let protocol_features = frontend.get_protocol_features().unwrap();
        assert!(protocol_features.contains(VhostUserProtocolFeatures::LOG_SHMFD));
        assert!(protocol_features.contains(VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS));
        frontend.set_features(features).unwrap();
        frontend
            .set_protocol_features(
                VhostUserProtocolFeatures::LOG_SHMFD
                    | VhostUserProtocolFeatures::CONFIGURE_MEM_SLOTS
                    | VhostUserProtocolFeatures::REPLY_ACK,
            )
            .unwrap();
        frontend.set_mem_table(&[initial_region.info()]).unwrap();
        frontend
            .set_log_base(
                0,
                Some(VhostUserDirtyLogRegion {
                    mmap_size: 4096,
                    mmap_offset: 0,
                    mmap_handle: log_fd,
                }),
            )
            .unwrap();

        let mut regions = vec![initial_region];
        for operation in operations {
            match operation {
                Operation::Add => {
                    let region = Region::new(regions.len());
                    frontend.add_mem_region(&region.info()).unwrap();
                    regions.push(region);
                }
                Operation::Replace {
                    regions: selected_regions,
                } => {
                    let infos = selected_regions
                        .iter()
                        .map(|index| regions[*index].info())
                        .collect::<Vec<_>>();
                    frontend.set_mem_table(&infos).unwrap();
                }
                Operation::Write { region, offset } => {
                    let target = &regions[region];
                    let atomic_memory = memory.lock().unwrap().as_ref().unwrap().clone();
                    atomic_memory
                        .memory()
                        .write_slice(&[0xa5], GuestAddress(target.guest_addr + offset as u64))
                        .unwrap();
                    let page = (target.guest_addr + offset as u64) / REGION_SIZE as u64;
                    let mut log_byte = [0];
                    log_file
                        .read_at(&mut log_byte, page / u8::BITS as u64)
                        .unwrap();
                    assert_ne!(
                        log_byte[0] & (1 << (page % u8::BITS as u64)),
                        0,
                        "write to GPA {:#x} did not dirty log page {page}",
                        target.guest_addr + offset as u64
                    );
                }
            }
        }
    });
    client.join().unwrap();
    server.join().unwrap();
}

#[hegel::test]
#[hegel::reproduce_failure("AXicY2MAAi5GIMEEoRhRKKggVAkDAAfkAE8=")]
fn hegel_backend_dirty_log_across_memory_changes(tc: TestCase) {
    let operations = draw_operations(tc);
    let socket_dir = tempfile::tempdir().unwrap();
    run_dirty_log_sequence(socket_dir.path().join("vhost-user.sock"), operations);
}
