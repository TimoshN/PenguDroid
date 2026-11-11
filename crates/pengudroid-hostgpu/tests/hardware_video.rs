//! Explicit hardware integration test. Supply a built plugin and the generated
//! one-frame fixture; this must fail rather than silently decode in software.
use pengudroid_hostgpu::video::{Decoder, HardwareFrame, ReceiveStatus, SendStatus};
use std::path::Path;
fn decode(decoder: &mut Decoder, bytes: &[u8], pts: i64) -> HardwareFrame {
    decoder.flush();
    assert_eq!(decoder.send(bytes, pts).unwrap(), SendStatus::Accepted);
    assert_eq!(decoder.end_of_stream().unwrap(), SendStatus::Accepted);
    match decoder.receive().unwrap() {
        ReceiveStatus::Frame(frame) => frame,
        _ => panic!("expected a hardware frame"),
    }
}
/// The fixture's first macroblock is Y=64, Cb=128, Cr=192. VA-API exports the
/// decode surface (NV12), so the luma sample itself is read back. Vulkan Video
/// exports the frame converted to RGBA: BT.601 limited range for a 128-line
/// stream gives (158, 4, 56), and GPU rounding may differ by one code.
fn assert_fixture_pixel(frame: &HardwareFrame, backend: &str) {
    if backend == "vulkan" {
        assert_eq!(frame.fourcc(), u32::from_le_bytes(*b"XB24"));
        assert_eq!(frame.planes().len(), 1);
        let pixels = frame.test_rgba().unwrap();
        assert_eq!(pixels.len(), 128 * 128 * 4);
        for (got, want) in pixels[..3].iter().zip([158i32, 4, 56]) {
            assert!((*got as i32 - want).abs() <= 1, "first pixel {:?}, expected (158, 4, 56)", &pixels[..4]);
        }
        assert_eq!(pixels[3], 255, "the exported frame is opaque");
    } else {
        assert_eq!(frame.fourcc(), u32::from_le_bytes(*b"NV12"));
        assert_eq!(frame.planes().len(), 2);
        assert_eq!(frame.test_luma().unwrap(), 64);
    }
}
#[test]
fn real_hardware_decode_export_backpressure_flush_and_cleanup() {
    let (Some(plugin), Some(fixture)) = (std::env::var_os("PENGUDROID_TEST_VIDEO_BRIDGE"),
        std::env::var_os("PENGUDROID_TEST_VIDEO_FIXTURE")) else {
        eprintln!("hardware video test skipped: set PENGUDROID_TEST_VIDEO_BRIDGE and PENGUDROID_TEST_VIDEO_FIXTURE");
        return;
    };
    let device = std::env::var_os("PENGUDROID_TEST_VIDEO_DEVICE").unwrap_or_else(|| "/dev/dri/renderD128".into());
    let bytes = std::fs::read(fixture).unwrap();
    let mut decoder = Decoder::open_plugin(Path::new(&plugin), "h264", Path::new(&device), &[]).unwrap();
    let backend = decoder.backend().expect("a current plugin reports its backend");
    println!("hardware video backend: {backend}");
    let mut held = Vec::new();
    for pts in 0..pengudroid_hostgpu::video::MAX_HELD_FRAMES as i64 {
        let frame = decode(&mut decoder, &bytes, pts * 1000);
        assert_eq!((frame.width(), frame.height()), (128, 128));
        assert_eq!(frame.pts_us(), pts * 1000);
        assert_fixture_pixel(&frame, backend);
        frame.export().unwrap().validate().unwrap();
        held.push(frame);
    }
    assert!(matches!(decoder.receive().unwrap(), ReceiveStatus::FramesHeld));
    held.clear();
    // Warm the driver/codec caches before checking descriptor retention.
    for pts in 0..5 { drop(decode(&mut decoder, &bytes, pts)); }
    let fd_count = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    let baseline = fd_count();
    let rss_kib = || std::fs::read_to_string("/proc/self/smaps_rollup").unwrap().lines()
        .find_map(|line| line.strip_prefix("Rss:").map(|v| v.split_whitespace().next().unwrap().parse::<u64>().unwrap())).unwrap();
    let baseline_rss = rss_kib();
    for pts in 0..2000 {
        let frame = decode(&mut decoder, &bytes, pts);
        let exported = frame.export().unwrap();
        assert!(!exported.objects.is_empty());
    }
    assert!(fd_count() <= baseline + 1, "GPU frame exports must not retain descriptors");
    let final_rss = rss_kib();
    println!("2,000 hardware frames: fd count {} -> {}, RSS {} -> {} KiB", baseline, fd_count(), baseline_rss, final_rss);
    assert!(final_rss <= baseline_rss + 16 * 1024, "GPU decode RSS must stay bounded after warmup");
    let frame = decode(&mut decoder, &bytes, 999);
    drop(decoder);
    // A frame retains its surface/device after decoder close.
    assert_fixture_pixel(&frame, backend);
    drop(frame);
    assert!(Decoder::open_plugin(Path::new(&plugin), "h264", Path::new("/dev/no-pd-gpu"), &[]).is_err());
    assert!(Decoder::open_plugin(Path::new(&plugin), "not-a-codec", Path::new(&device), &[]).is_err());
}
