use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::thread;

use hegel::generators as gs;
use hegel::TestCase;

use super::*;

fn set_socket_buffer(stream: &UnixStream, option: libc::c_int) {
    let size: libc::c_int = 256;
    let result = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of_val(&size) as libc::socklen_t,
        )
    };
    assert_eq!(result, 0);
}

fn draw_iov_lengths(tc: &TestCase, total: usize) -> Vec<usize> {
    let split_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(64));
    let mut boundaries = Vec::with_capacity(split_count + 2);
    boundaries.push(0);
    boundaries.push(total);
    for _ in 0..split_count {
        boundaries.push(tc.draw(gs::integers::<usize>().min_value(0).max_value(total)));
    }
    boundaries.sort_unstable();
    let mut lengths = boundaries
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .collect::<Vec<_>>();
    let zero_count = tc.draw(gs::integers::<usize>().min_value(0).max_value(32));
    lengths.extend(std::iter::repeat_n(0, zero_count));
    lengths
}

#[hegel::test]
fn hegel_endpoint_iovec_transfer(tc: TestCase) {
    let payload_len = tc.draw(gs::integers::<usize>().min_value(0).max_value(8192));
    let payload = (0..payload_len)
        .map(|_| tc.draw(gs::integers::<u8>()))
        .collect::<Vec<_>>();
    let send_lengths = draw_iov_lengths(&tc, payload.len());
    let receive_lengths = draw_iov_lengths(&tc, payload.len());
    let expected_payload = payload.clone();
    let (send_socket, receive_socket) = UnixStream::pair().unwrap();
    set_socket_buffer(&send_socket, libc::SO_SNDBUF);
    set_socket_buffer(&receive_socket, libc::SO_RCVBUF);
    let mut sender = Endpoint::<VhostUserMsgHeader<FrontendReq>>::from_stream(send_socket);
    let mut receiver = Endpoint::<VhostUserMsgHeader<FrontendReq>>::from_stream(receive_socket);
    let sender_thread = thread::spawn(move || {
        let mut cursor = 0;
        let send_iovs = send_lengths
            .iter()
            .map(|length| {
                let iov = &payload[cursor..cursor + length];
                cursor += length;
                iov
            })
            .collect::<Vec<_>>();
        assert_eq!(cursor, payload.len());
        sender.send_iovec_all(&send_iovs, None).unwrap()
    });

    let mut buffers = receive_lengths
        .iter()
        .map(|length| vec![0; *length])
        .collect::<Vec<_>>();
    let mut receive_iovs = buffers
        .iter_mut()
        .map(|buffer| iovec {
            iov_base: buffer.as_mut_ptr().cast(),
            iov_len: buffer.len(),
        })
        .collect::<Vec<_>>();
    let (received, files) = unsafe { receiver.recv_into_iovec_all(&mut receive_iovs).unwrap() };
    let sent = sender_thread.join().unwrap();
    assert!(files.is_none());
    assert_eq!(sent, expected_payload.len());
    assert_eq!(received, expected_payload.len());
    assert_eq!(buffers.concat(), expected_payload);
}

#[hegel::test]
fn hegel_endpoint_sub_iovs_offset(tc: TestCase) {
    let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(128));
    let lengths = (0..count)
        .map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(4096)))
        .collect::<Vec<_>>();
    let total = lengths.iter().sum::<usize>();
    let skipped = tc.draw(gs::integers::<usize>().min_value(0).max_value(total + 1));

    let mut remaining = skipped;
    let mut expected_index = 0;
    while expected_index < lengths.len() && remaining >= lengths[expected_index] {
        remaining -= lengths[expected_index];
        expected_index += 1;
    }

    assert_eq!(
        get_sub_iovs_offset(&lengths, skipped),
        (expected_index, remaining)
    );
}
