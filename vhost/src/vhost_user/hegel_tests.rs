use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;

use hegel::generators as gs;
use hegel::TestCase;

use super::dummy_backend::{DummyBackendReqHandler, VIRTIO_FEATURES};
use super::message::{
    FrontendReq, VhostUserConfigFlags, VhostUserHeaderFlag, VhostUserProtocolFeatures,
    VhostUserVirtioFeatures, MAX_MSG_SIZE, VHOST_USER_CONFIG_OFFSET, VHOST_USER_CONFIG_SIZE,
};
use super::*;
use crate::backend::VhostBackend;

fn write_request(stream: &mut UnixStream, request: u32, flags: u32, body: &[u8]) {
    stream.write_all(&request.to_le_bytes()).unwrap();
    stream.write_all(&flags.to_le_bytes()).unwrap();
    stream
        .write_all(&(body.len() as u32).to_le_bytes())
        .unwrap();
    stream.write_all(body).unwrap();
}

fn read_message(stream: &mut UnixStream) -> Vec<u8> {
    let mut header = [0; 12];
    stream.read_exact(&mut header).unwrap();
    let size = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
    let mut message = header.to_vec();
    message.resize(12 + size, 0);
    stream.read_exact(&mut message[12..]).unwrap();
    message
}

fn negotiate_reply_ack(
    frontend: &mut UnixStream,
    handler: &mut BackendReqHandler<Mutex<DummyBackendReqHandler>>,
) {
    write_request(frontend, FrontendReq::GET_FEATURES.into(), 1, &[]);
    handler.handle_request().unwrap();
    read_message(frontend);

    write_request(frontend, FrontendReq::SET_OWNER.into(), 1, &[]);
    handler.handle_request().unwrap();

    write_request(
        frontend,
        FrontendReq::SET_FEATURES.into(),
        1,
        &VIRTIO_FEATURES.to_le_bytes(),
    );
    handler.handle_request().unwrap();

    write_request(frontend, FrontendReq::GET_PROTOCOL_FEATURES.into(), 1, &[]);
    handler.handle_request().unwrap();
    read_message(frontend);

    write_request(
        frontend,
        FrontendReq::SET_PROTOCOL_FEATURES.into(),
        1 | VhostUserHeaderFlag::NEED_REPLY.bits(),
        &VhostUserProtocolFeatures::all().bits().to_le_bytes(),
    );
    handler.handle_request().unwrap();
    let reply = read_message(frontend);
    assert_eq!(
        u32::from_le_bytes(reply[4..8].try_into().unwrap()) & VhostUserHeaderFlag::REPLY.bits(),
        VhostUserHeaderFlag::REPLY.bits()
    );
}

fn draw_flags(tc: &TestCase) -> u32 {
    match tc.draw(gs::integers::<u8>().min_value(0).max_value(9)) {
        0 => 1,
        1 => 1 | VhostUserHeaderFlag::NEED_REPLY.bits(),
        2 => 1 | VhostUserHeaderFlag::REPLY.bits(),
        3 => 1 | VhostUserHeaderFlag::REPLY.bits() | VhostUserHeaderFlag::NEED_REPLY.bits(),
        4 => 0,
        5 => 2,
        6 => 3,
        7 => 0x81,
        8 => 0xffff,
        _ => 0x100,
    }
}

#[hegel::test]
fn hegel_backend_request_handler_raw_messages(tc: TestCase) {
    let (mut frontend, backend) = UnixStream::pair().unwrap();
    let dummy = Arc::new(Mutex::new(DummyBackendReqHandler::new()));
    let mut handler = BackendReqHandler::from_stream(backend, dummy);
    let negotiated = tc.draw(gs::booleans());
    if negotiated {
        negotiate_reply_ack(&mut frontend, &mut handler);
    }

    let request = tc.draw(gs::integers::<u32>().min_value(0).max_value(50));
    let flags = draw_flags(&tc);
    let size = if tc.draw(gs::booleans()) {
        tc.draw(gs::integers::<usize>().min_value(0).max_value(128))
    } else {
        tc.draw(
            gs::integers::<usize>()
                .min_value(MAX_MSG_SIZE.saturating_sub(64))
                .max_value(MAX_MSG_SIZE),
        )
    };
    let body = (0..size)
        .map(|_| tc.draw(gs::integers::<u8>()))
        .collect::<Vec<_>>();
    write_request(&mut frontend, request, flags, &body);
    let handled = handler.handle_request();

    if negotiated && flags & VhostUserHeaderFlag::NEED_REPLY.bits() != 0 && handled.is_ok() {
        frontend.set_nonblocking(true).unwrap();
        let mut replies = Vec::new();
        let mut buffer = [0; MAX_MSG_SIZE + 12];
        loop {
            match frontend.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => replies.extend_from_slice(&buffer[..size]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("{error}"),
            }
        }
        assert!(replies.len() >= 12);
        let reply_code = u32::from_le_bytes(replies[0..4].try_into().unwrap());
        let reply_flags = u32::from_le_bytes(replies[4..8].try_into().unwrap());
        let reply_size = u32::from_le_bytes(replies[8..12].try_into().unwrap()) as usize;
        assert_eq!(reply_code, request);
        assert_eq!(reply_flags & 3, 1);
        assert_ne!(reply_flags & VhostUserHeaderFlag::REPLY.bits(), 0);
        assert_eq!(reply_flags & VhostUserHeaderFlag::RESERVED_BITS.bits(), 0);
        assert!(reply_size <= MAX_MSG_SIZE);
        assert_eq!(replies.len(), 12 + reply_size);
    }
}

struct FrontendModel {
    frontend: Frontend,
    backend: Arc<Mutex<DummyBackendReqHandler>>,
    config: [u8; VHOST_USER_CONFIG_SIZE as usize],
    features: u64,
    features_set: bool,
    vring_num: [u16; 2],
    vring_base: [u32; 2],
    vring_enabled: [bool; 2],
}

#[hegel::state_machine]
impl FrontendModel {
    #[rule]
    fn set_config(&mut self, _tc: TestCase) {
        let offset = VHOST_USER_CONFIG_OFFSET as usize;
        let payload = self.config[offset..offset + 4].to_vec();
        self.frontend
            .set_config(
                VHOST_USER_CONFIG_OFFSET,
                VhostUserConfigFlags::empty(),
                &payload,
            )
            .unwrap();
    }

    #[rule]
    fn get_config(&mut self, _tc: TestCase) {
        let (config, data) = self
            .frontend
            .get_config(
                VHOST_USER_CONFIG_OFFSET,
                4,
                VhostUserConfigFlags::empty(),
                &[0; 4],
            )
            .unwrap();
        let offset = config.offset;
        let size = config.size;
        assert_eq!(offset, VHOST_USER_CONFIG_OFFSET);
        assert_eq!(size, 4);
        assert_eq!(
            data,
            self.config[offset as usize..offset as usize + size as usize]
        );
    }

    #[rule]
    fn set_vring_num(&mut self, tc: TestCase) {
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
        let num = tc.draw(gs::integers::<u16>().min_value(1).max_value(256));
        self.frontend.set_vring_num(index, num).unwrap();
        self.vring_num[index] = num;
        assert_eq!(
            self.backend.lock().unwrap().vring_num[index],
            u32::from(num)
        );
    }

    #[rule]
    fn set_and_get_vring_base(&mut self, tc: TestCase) {
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
        let base = tc.draw(gs::integers::<u16>().min_value(0).max_value(255));
        self.frontend.set_vring_base(index, base).unwrap();
        self.vring_base[index] = u32::from(base);
        assert_eq!(
            self.frontend.get_vring_base(index).unwrap(),
            u32::from(base)
        );
    }

    #[rule]
    fn enable_vring(&mut self, tc: TestCase) {
        if !self.features_set {
            return;
        }
        let index = tc.draw(gs::integers::<usize>().min_value(0).max_value(1));
        let enabled = tc.draw(gs::booleans());
        self.frontend.set_vring_enable(index, enabled).unwrap();
        self.vring_enabled[index] = enabled;
        assert_eq!(self.backend.lock().unwrap().vring_enabled[index], enabled);
    }

    #[rule]
    fn set_features(&mut self, tc: TestCase) {
        if self.features_set {
            return;
        }
        self.features = tc.draw(gs::integers::<u64>()) & VIRTIO_FEATURES
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits();
        self.frontend.set_features(self.features).unwrap();
        self.features_set = true;
        self.vring_enabled = [false; 2];
    }

    #[rule]
    fn get_features(&mut self, _tc: TestCase) {
        assert_eq!(self.frontend.get_features().unwrap(), VIRTIO_FEATURES);
    }

    #[invariant]
    fn backend_vring_state_matches_model(&self, _tc: TestCase) {
        let backend = self.backend.lock().unwrap();
        assert_eq!(backend.vring_num, self.vring_num.map(u32::from));
        assert_eq!(backend.vring_base, self.vring_base);
        assert_eq!(backend.vring_enabled, self.vring_enabled);
    }
}

#[hegel::test]
fn hegel_frontend_backend_stateful_model(tc: TestCase) {
    let (frontend_socket, backend_socket) = UnixStream::pair().unwrap();
    let backend = Arc::new(Mutex::new(DummyBackendReqHandler::new()));
    let backend_handler = backend.clone();
    let server = thread::spawn(move || {
        let mut handler = BackendReqHandler::from_stream(backend_socket, backend_handler);
        while handler.handle_request().is_ok() {}
    });

    let mut frontend = Frontend::from_stream(frontend_socket, 2);
    frontend.set_owner().unwrap();
    let features = frontend.get_features().unwrap();
    assert_eq!(features, VIRTIO_FEATURES);
    frontend
        .set_protocol_features(
            VhostUserProtocolFeatures::CONFIG | VhostUserProtocolFeatures::REPLY_ACK,
        )
        .unwrap();
    frontend.set_hdr_flags(VhostUserHeaderFlag::NEED_REPLY);

    hegel::stateful::machine(FrontendModel {
        frontend,
        backend: backend.clone(),
        config: {
            let mut config = [0; VHOST_USER_CONFIG_SIZE as usize];
            config[VHOST_USER_CONFIG_OFFSET as usize..VHOST_USER_CONFIG_OFFSET as usize + 4]
                .fill(0xa5);
            config
        },
        features: 0,
        features_set: false,
        vring_num: [0; 2],
        vring_base: [0; 2],
        vring_enabled: [false; 2],
    })
    .run(tc);
    server.join().unwrap();
}
