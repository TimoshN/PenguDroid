//! Vulkan windows must feed the same Android event queue as GL windows.
use pengudroid_proto::{InputKind, Message, PROTO_VERSION};
use pengudroid_socket::Connection;
use std::os::unix::net::UnixListener;
use std::time::{Duration, Instant};

#[test]
fn non_gl_surface_subscribes_and_delivers_pointer_input() {
    let path = std::env::temp_dir().join(format!("pd-vulkan-input-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path).unwrap();
    std::env::set_var("PENGUDROID_SOCKET", &path);
    let server = std::thread::spawn(move || {
        let connection = Connection::from_stream(listener.accept().unwrap().0);
        connection.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        assert!(matches!(connection.recv_message().unwrap().0, Message::Hello { .. }));
        connection.send_message(&Message::Welcome { version: PROTO_VERSION, pid: 1 }, &[]).unwrap();
        assert!(matches!(connection.recv_message().unwrap().0, Message::SubscribeInput { surface_id: 42 }));
        connection.send_message(&Message::Ack, &[]).unwrap();
        connection.send_message(&Message::InputEvent { surface_id: 42,
            event: InputKind::PointerMotion { x: 81.0, y: 123.0 } }, &[]).unwrap();
    });
    pengudroid_gleshim::pengudroid_subscribe_input(42);
    let mut bytes = [0u8; 16];
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { pengudroid_gleshim::pengudroid_input_poll(bytes.as_mut_ptr()) } == 0 {
        assert!(Instant::now() < deadline, "Vulkan input never reached Android queue");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(u32::from_le_bytes(bytes[..4].try_into().unwrap()), 42);
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 3);
    assert_eq!(f32::from_le_bytes(bytes[8..12].try_into().unwrap()), 81.0);
    assert_eq!(f32::from_le_bytes(bytes[12..16].try_into().unwrap()), 123.0);
    assert_eq!(std::env::var("PENGUDROID_LAST_SURFACE_ID").unwrap(), "42");
    server.join().unwrap();
    std::fs::remove_file(path).unwrap();
}
