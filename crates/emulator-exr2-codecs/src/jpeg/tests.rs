use super::*;
use fujimod_emulator_devices::Device;
use fujimod_emulator_imaging::exr2_isp::{Irq, stream::{State, StreamHandle}};
use fujimod_emulator_devices::test_host::{TestHost, advance, advance_from_checkpoint, read, write};
fn encoder(stream: StreamHandle) -> Encoder {
    Encoder::new(
        encoder::Options {
            completion_ticks: 10,
            reset_ticks: 2,
        },
        (Irq { core: 0, line: 77 }, stream),
    )
    .unwrap()
}
fn configure(d: &mut Encoder, h: &mut TestHost, stream: bool) {
    for (o, v) in [
        (0, 4),
        (0x28, 0xfd0000),
        (0x40, 1),
        (0x44, 8 << 16 | 16),
        (0x80, 4),
        (0x84, 0x100),
    ] {
        write(d, h, 0xfff62000 + o, v);
    }
    for _ in 0..32 {
        write(d, h, 0xfff6208c, 0x03030303);
    }
    write(d, h, 0xfff62084, 1);
    for (o, v) in [
        (0x100, 0x8000),
        (0x10c, 0x9000),
        (0x110, 4096),
        (0x120, if stream { 2 } else { 0 }),
    ] {
        write(d, h, 0xfff62000 + o, v);
    }
}
// ImageMagick `-size 36x22 gradient:black-white -strip -quality 60`, 197 bytes,
// sha256 f17324773f37349248950a6d0f471e97b0a297ca5c8c21f25dc804ac78b53c8f.
const JPEG_36X22: &[u8] = b"\xff\xd8\xff\xe0\x00\x10\x4a\x46\x49\x46\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00\xff\xdb\x00\x43\x00\x0d\x09\x0a\x0b\x0a\x08\x0d\x0b\x0a\x0b\x0e\x0e\x0d\x0f\x13\x20\x15\x13\x12\x12\x13\x27\x1c\x1e\x17\x20\x2e\x29\x31\x30\x2e\x29\x2d\x2c\x33\x3a\x4a\x3e\x33\x36\x46\x37\x2c\x2d\x40\x57\x41\x46\x4c\x4e\x52\x53\x52\x32\x3e\x5a\x61\x5a\x50\x60\x4a\x51\x52\x4f\xff\xc0\x00\x0b\x08\x00\x16\x00\x24\x01\x01\x11\x00\xff\xc4\x00\x15\x00\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x06\xff\xc4\x00\x18\x10\x00\x03\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x15\x62\x03\x61\xff\xda\x00\x08\x01\x01\x00\x00\x3f\x00\x8a\x53\x21\x4c\x85\x32\x14\xc8\x53\x25\xe2\x99\x0a\x64\x29\x90\xa6\x42\x99\x2e\x57\x67\xc0\xbb\x3e\x05\xd9\xf0\x2e\xcf\x81\x76\x7c\x3f\xff\xd9";
const JPEG_SOURCE: u64 = 0x9000;
fn jpeg_decoder() -> Decoder {
    Decoder::new(
        decoder::Options {
            completion_ticks: 5,
            reset_ticks: 2,
        },
        Irq { core: 0, line: 76 },
    )
    .unwrap()
}
fn load_jpeg(h: &mut TestHost) {
    h.ram[JPEG_SOURCE as usize..JPEG_SOURCE as usize + JPEG_36X22.len()]
        .copy_from_slice(JPEG_36X22);
}
fn start_jpeg(d: &mut Decoder, h: &mut TestHost, destination: u32, shift: u32) {
    for (o, v) in [
        (8, 0xfd0020),
        (0xa0, shift),
        (0x100, destination),
        (0x104, 0x1000),
        (0x10c, JPEG_SOURCE as u32),
        (0x110, JPEG_36X22.len() as u32),
        (0, 1),
    ] {
        write(d, h, 0xfff63000 + o, v);
    }
}
#[test]
fn memory_encode_reserves_prefix_and_decoder_publishes_pixels_before_irq() {
    let mut h = TestHost::new();
    h.ram[0x8000..0x8100].fill(128);
    h.ram[0x9000..0xa000].fill(0xa5);
    let mut d = encoder(Default::default());
    configure(&mut d, &mut h, false);
    write(&mut d, &mut h, 0xfff62000, 5);
    advance(&mut d, &mut h, 9);
    assert!(h.effects.is_empty());
    advance_from_checkpoint(&mut d, &mut h, 10);
    let count = read(&mut d, &mut h, 0xfff62094) as u32;
    assert!(count > 500);
    assert_eq!(&h.ram[0x9000..0x9004], &[0xa5, 0xa5, 255, 0xc4]);
    assert_eq!(h.effects, ["dma", "irq"]);
    assert_eq!(read(&mut d, &mut h, 0xfff62024), 16);
    let mut decoder = Decoder::new(
        decoder::Options {
            completion_ticks: 5,
            reset_ticks: 2,
        },
        Irq { core: 0, line: 76 },
    )
    .unwrap();
    for (o, v) in [
        (8, 0xfd0020),
        (0x100, 0xa000),
        (0x104, 256),
        (0x10c, 0x9000),
        (0x110, count),
        (0, 1),
    ] {
        write(&mut decoder, &mut h, 0xfff63000 + o, v);
    }
    advance(&mut decoder, &mut h, 14);
    assert_eq!(h.irqs.len(), 1);
    advance_from_checkpoint(&mut decoder, &mut h, 15);
    assert_eq!(&h.ram[0xa000..0xa004], &[128, 128, 128, 128]);
    assert_eq!(h.irqs, [(0, 77), (0, 76)]);
    // 16x8 packed422 at pitch 16 writes 256 bytes.
    assert_eq!(read(&mut decoder, &mut h, 0xfff63108), 0xa000 + 16 * 8 * 2);
    write(&mut decoder, &mut h, 0xfff63004, 16);
    assert_eq!(read(&mut decoder, &mut h, 0xfff63004), 0);
    write(&mut decoder, &mut h, 0xfff63118, 1);
    advance(&mut decoder, &mut h, 16);
    assert_eq!(read(&mut decoder, &mut h, 0xfff63004), 0);
    advance(&mut decoder, &mut h, 17);
    assert_eq!(read(&mut decoder, &mut h, 0xfff63004), 0x10000);
}
#[test]
fn encode_capacity_failure_does_not_claim_stream_or_write_ram() {
    let mut h = TestHost::new();
    let stream: StreamHandle = Default::default();
    {
        let mut s = stream.lock().unwrap();
        s.state = State::Ready;
        s.width = 16;
        s.height = 8;
        s.pixels = vec![128; 256].into();
    }
    let mut e = encoder(stream.clone());
    configure(&mut e, &mut h, true);
    write(&mut e, &mut h, 0xfff62110, 8);
    let mut cx = h.cx();
    e.write(&mut cx, 0xfff62000, 4, 5);
    assert!(cx.refusal().is_some());
    assert_eq!(stream.lock().unwrap().state, State::Ready);
    assert!(h.effects.is_empty());
    assert_eq!(e.next_deadline(0), None);
}

#[test]
fn unselected_quantization_is_retained_without_fifo_dma_or_irq() {
    let mut h = TestHost::new();
    let mut d = encoder(Default::default());
    configure(&mut d, &mut h, false);
    let uploaded = d.save()[1].clone();
    assert_eq!(d.save()[2], 128);
    write(&mut d, &mut h, 0xfff62084, 0);
    assert_eq!(d.save()[1], uploaded);
    assert_eq!(d.save()[2], 128);
    assert_eq!(d.save()[0][0x84 / 4], 0);
    assert_eq!(d.next_deadline(0), None);
    let before = d.fingerprint().unwrap();
    advance(&mut d, &mut h, 10000);
    assert_eq!(d.fingerprint().unwrap(), before);
    assert!(h.effects.is_empty());
    let mut cx = h.cx();
    d.write(&mut cx, 0xfff6208c, 4, 0x03030303);
    assert!(cx.refusal().unwrap().contains("outside FIFO"));
    assert_eq!(d.fingerprint().unwrap(), before);
    write(&mut d, &mut h, 0xfff62084, 1);
    assert_eq!(d.save()[1], uploaded);
    assert_eq!(d.save()[3], true);
    assert!(h.effects.is_empty());
    write(&mut d, &mut h, 0xfff62000, 5);
    advance(&mut d, &mut h, 10010);
    assert!(read(&mut d, &mut h, 0xfff62094) > 2);
    assert_eq!(h.effects, ["dma", "irq"]);
}

#[test]
fn programmed_restart_nine_reaches_the_real_codestream_and_decoder() {
    // Restart interval 9 emits ten cyclic RST markers for a 128x96 4:2:2
    // image (96 MCUs).
    let mut h = TestHost::new();
    h.ram[0x8000..0xe000].fill(128);
    h.ram[0x10000..0x14000].fill(0xa5);
    h.ram[0x10000..0x10002].copy_from_slice(&[255, 0xd8]);
    let mut d = encoder(Default::default());
    configure(&mut d, &mut h, false);
    for (o, v) in [
        (0x44, 96 << 16 | 128),
        (0x80, 9),
        (0x10c, 0x10000),
        (0x110, 0x4000),
    ] {
        write(&mut d, &mut h, 0xfff62000 + o, v);
    }
    write(&mut d, &mut h, 0xfff62000, 5);
    advance(&mut d, &mut h, 9);
    assert_eq!(&h.ram[0x10002..0x10006], &[0xa5; 4]);
    assert!(h.effects.is_empty());
    advance(&mut d, &mut h, 10);
    let bytes = read(&mut d, &mut h, 0xfff62094) as usize;
    let encoded = &h.ram[0x10000..0x10000 + bytes];
    assert!(encoded.windows(6).any(|v| v == [255, 0xdd, 0, 4, 0, 9]));
    let markers = encoded
        .windows(2)
        .filter(|v| v[0] == 255 && (0xd0..=0xd7).contains(&v[1]))
        .map(|v| v[1] - 0xd0)
        .collect::<Vec<_>>();
    assert_eq!(markers, (0..10).map(|v| v % 8).collect::<Vec<_>>());
    let options = zune_core::options::DecoderOptions::new_safe().set_use_unsafe(false);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
        zune_core::bytestream::ZCursor::new(encoded),
        options,
    );
    let rgb = decoder.decode().unwrap();
    assert_eq!(rgb.len(), 128 * 96 * 3);
    assert!(rgb.iter().all(|v| (127..=129).contains(v)));
    assert_eq!(h.effects, ["dma", "irq"]);
    assert_eq!(h.irqs, [(0, 77)]);
}

#[test]
fn jpeg_completion_cursor_is_the_padded_extent_of_the_programmed_destination() {
    let mut h = TestHost::new();
    h.ram[0x80000..0x82000].fill(0xa5);
    load_jpeg(&mut h);
    let mut d = jpeg_decoder();
    start_jpeg(&mut d, &mut h, 0x4008_0000, 0);
    advance(&mut d, &mut h, 5);
    assert_eq!(h.effects, ["dma", "irq"]);
    assert_eq!(h.irqs, [(0, 76)]);
    let programmed = read(&mut d, &mut h, 0xfff63100);
    assert_eq!(programmed, 0x4008_0000);
    let cursor = read(&mut d, &mut h, 0xfff63108);
    assert_eq!(cursor, 0x4008_0000 + 48 * 24 * 2);
    assert_eq!(cursor - programmed, 48 * 24 * 2);
    assert_eq!(read(&mut d, &mut h, 0xfff63110), JPEG_36X22.len() as u64);
    assert_ne!(JPEG_36X22.len(), 48 * 24 * 2);
    let dest = &h.ram[0x80000..0x80000 + 48 * 24 * 2];
    assert!(dest.iter().any(|&v| v != 0));
    for y in 0..24 {
        let row = &dest[y * 48 * 2..(y + 1) * 48 * 2];
        assert!(row[36 * 2..].iter().all(|&v| v == 0), "row {y} pitch padding");
        if y >= 22 {
            assert!(row.iter().all(|&v| v == 0), "padding row {y}");
        }
    }
    for y in 0..22 {
        for x in (0..36).step_by(2) {
            let at = y * 48 * 2 + x * 2;
            assert_eq!((dest[at + 2], dest[at + 3]), (128, 128), "pair {y}/{x}");
        }
    }
    assert_eq!(&h.ram[0x80000 + 48 * 24 * 2..][..4], &[0xa5; 4]);
}

#[test]
fn jpeg_downscale_cursor_uses_the_scaled_padded_extent() {
    let mut h = TestHost::new();
    h.ram[0x80000..0x82000].fill(0xa5);
    load_jpeg(&mut h);
    let mut d = jpeg_decoder();
    start_jpeg(&mut d, &mut h, 0x4008_1000, 1);
    advance(&mut d, &mut h, 5);
    assert_eq!(read(&mut d, &mut h, 0xfff630a0), 1);
    let cursor = read(&mut d, &mut h, 0xfff63108);
    assert_eq!(cursor, 0x4008_1000 + 32 * 16 * 2);
    assert_eq!(cursor - 0x4008_1000, 32 * 16 * 2);
    let dest = &h.ram[0x81000..0x81000 + 32 * 16 * 2];
    assert!(dest.iter().any(|&v| v != 0));
    for y in 0..16 {
        let row = &dest[y * 64..(y + 1) * 64];
        assert!(row[36..].iter().all(|&v| v == 0), "row {y} pitch padding");
        if y >= 11 {
            assert!(row.iter().all(|&v| v == 0), "padding row {y}");
        }
    }
    for y in 0..11 {
        for x in (0..18).step_by(2) {
            let at = y * 64 + x * 2;
            assert_eq!((dest[at + 2], dest[at + 3]), (128, 128), "pair {y}/{x}");
        }
    }
    assert_eq!(&h.ram[0x81000 + 32 * 16 * 2..][..4], &[0xa5; 4]);
}

#[test]
fn unsuccessful_jpeg_dma_publishes_no_completion_cursor_or_irq() {
    let mut h = TestHost::new();
    load_jpeg(&mut h);
    let mut d = jpeg_decoder();
    // 0x4030_0000 normalizes into the alias aperture but past this host's 1 MiB.
    start_jpeg(&mut d, &mut h, 0x4030_0000, 0);
    h.now = 5;
    let mut cx = h.cx();
    d.advance(&mut cx);
    assert!(cx.refusal().unwrap().contains("unmapped"));
    assert_eq!(read(&mut d, &mut h, 0xfff63108), 0);
    assert_eq!(read(&mut d, &mut h, 0xfff63004), 0);
    assert!(h.irqs.is_empty());
    assert!(h.effects.is_empty());
    assert_eq!(d.next_deadline(5), Some(5));
    h.now = 6;
    let mut cx = h.cx();
    d.advance(&mut cx);
    assert!(cx.refusal().is_some());
    assert_eq!(read(&mut d, &mut h, 0xfff63108), 0);
    assert!(h.irqs.is_empty());
}

#[test]
fn jpeg_checkpoint_resumes_the_pending_cursor_and_bytes_once() {
    let mut h = TestHost::new();
    h.ram[0x80000..0x82000].fill(0xa5);
    load_jpeg(&mut h);
    let mut d = jpeg_decoder();
    start_jpeg(&mut d, &mut h, 0x4008_0000, 0);
    assert_eq!(d.next_deadline(0), Some(5));
    advance_from_checkpoint(&mut d, &mut h, 5);
    assert_eq!(read(&mut d, &mut h, 0xfff63108), 0x4008_0000 + 48 * 24 * 2);
    assert_eq!(h.effects, ["dma", "irq"]);
    assert_eq!(h.irqs, [(0, 76)]);
    assert_eq!(d.next_deadline(5), None);
    assert!(h.ram[0x80000..0x80000 + 48 * 24 * 2].iter().any(|&v| v != 0));
    assert_eq!(&h.ram[0x80000 + 48 * 24 * 2..][..4], &[0xa5; 4]);
}

#[test]
fn reset_is_refused_while_pending_and_retains_the_completed_cursor() {
    let mut h = TestHost::new();
    h.ram[0x80000..0x82000].fill(0xa5);
    load_jpeg(&mut h);
    let mut d = jpeg_decoder();
    start_jpeg(&mut d, &mut h, 0x4008_0000, 0);
    let mut cx = h.cx();
    d.write(&mut cx, 0xfff63118, 4, 1);
    assert!(cx.refusal().unwrap().contains("busy"));
    assert_eq!(d.next_deadline(0), Some(5));
    assert_eq!(read(&mut d, &mut h, 0xfff63118), 0);
    assert!(h.irqs.is_empty());
    assert!(h.ram[0x80000..0x82000].iter().all(|&v| v == 0xa5));
    advance(&mut d, &mut h, 5);
    let first = read(&mut d, &mut h, 0xfff63108);
    assert_eq!(first, 0x4008_0000 + 48 * 24 * 2);
    assert_eq!(h.irqs, [(0, 76)]);
    write(&mut d, &mut h, 0xfff63004, 0x10);
    assert_eq!(read(&mut d, &mut h, 0xfff63004), 0);
    write(&mut d, &mut h, 0xfff63118, 1);
    advance(&mut d, &mut h, 7);
    assert_eq!(read(&mut d, &mut h, 0xfff63004), 0x10000);
    assert_eq!(read(&mut d, &mut h, 0xfff63118), 0);
    assert_eq!(read(&mut d, &mut h, 0xfff63108), first);
    assert_eq!(h.irqs, [(0, 76)]);
    start_jpeg(&mut d, &mut h, 0x4008_1000, 1);
    assert_eq!(d.next_deadline(7), Some(12));
    assert_eq!(read(&mut d, &mut h, 0xfff63108), first);
    assert_eq!(&h.ram[0x81000..0x81004], &[0xa5; 4]);
    advance(&mut d, &mut h, 12);
    let second = read(&mut d, &mut h, 0xfff63108);
    assert_eq!(second, 0x4008_1000 + 32 * 16 * 2);
    assert_ne!(second, first);
    assert_eq!(read(&mut d, &mut h, 0xfff63004), 0x10010);
    assert_eq!(h.irqs, [(0, 76), (0, 76)]);
}

#[test]
fn unqualified_source_field_names_its_register_and_refuses_before_any_effect() {
    let mut h = TestHost::new();
    let mut d = encoder(Default::default());
    configure(&mut d, &mut h, false);
    write(&mut d, &mut h, 0xfff62048, 0x12);
    let before = d.fingerprint().unwrap();
    let mut cx = h.cx();
    d.write(&mut cx, 0xfff62000, 4, 5);
    let message = cx.refusal().unwrap();
    assert!(message.contains("0xfff62048=0x12"));
    assert_eq!(d.fingerprint().unwrap(), before);
    assert!(h.effects.is_empty());
    assert_eq!(d.next_deadline(0), None);
}

#[test]
fn implicit_and_explicit_packed_input_modes_encode_the_same_addressed_pixels() {
    let mut encodings = Vec::new();
    for mode in [0, 0x11] {
        let mut h = TestHost::new();
        h.ram[0x8000..0x8100].fill(128);
        let mut d = encoder(Default::default());
        configure(&mut d, &mut h, false);
        write(&mut d, &mut h, 0xfff62048, mode);
        write(&mut d, &mut h, 0xfff62000, 5);
        assert_eq!(d.next_deadline(0), Some(10));
        advance(&mut d, &mut h, 9);
        assert!(h.effects.is_empty());
        advance(&mut d, &mut h, 10);
        let count = read(&mut d, &mut h, 0xfff62094) as usize;
        assert!(count > 500);
        assert_eq!(h.effects, ["dma", "irq"]);
        assert_eq!(h.irqs, [(0, 77)]);
        encodings.push(h.ram[0x9000..0x9000 + count].to_vec());
    }
    assert_eq!(encodings[0], encodings[1]);
}

#[test]
fn worker_codec_retains_exact_stream_bytes_error_state_and_completion() {
    fujimod_emulator_devices::work::configure(2).unwrap();
    memory_encode_reserves_prefix_and_decoder_publishes_pixels_before_irq();
    encode_capacity_failure_does_not_claim_stream_or_write_ram();
    programmed_restart_nine_reaches_the_real_codestream_and_decoder();
    implicit_and_explicit_packed_input_modes_encode_the_same_addressed_pixels();
    fujimod_emulator_devices::work::configure(0).unwrap();
}

#[test]
fn stream_encode_start_retains_pending_warp_without_joining_or_publishing() {
    use std::sync::mpsc;
    fujimod_emulator_devices::work::configure(2).unwrap();
    let (release, held) = mpsc::channel();
    let (started, running) = mpsc::channel();
    let stream: StreamHandle = Default::default();
    {
        let mut s = stream.lock().unwrap();
        s.state = State::Ready;
        s.width = 16;
        s.height = 8;
        s.pixels.defer(move |_| {
            started.send(()).unwrap();
            held.recv().unwrap();
            vec![128; 256]
        });
    }
    running.recv().unwrap();
    let mut h = TestHost::new();
    h.ram[0x9000..0xa000].fill(0xa5);
    let mut e = encoder(stream.clone());
    configure(&mut e, &mut h, true);
    write(&mut e, &mut h, 0xfff62000, 5);
    assert_eq!(stream.lock().unwrap().state, State::Claimed);
    assert!(h.effects.is_empty());
    assert!(h.ram[0x9000..0xa000].iter().all(|&v| v == 0xa5));
    release.send(()).unwrap();
    advance_from_checkpoint(&mut e, &mut h, 9);
    assert!(h.effects.is_empty());
    advance(&mut e, &mut h, 10);
    assert_eq!(h.effects, ["dma", "irq"]);
    assert_eq!(stream.lock().unwrap().state, State::Finished);
    assert_eq!(&h.ram[0x9000..0x9004], &[0xa5, 0xa5, 255, 0xc4]);
    fujimod_emulator_devices::work::configure(0).unwrap();
}

#[test]
fn deferred_entropy_overflow_survives_checkpoint_and_completes_without_ready_or_ram() {
    fujimod_emulator_devices::work::configure(2).unwrap();
    let stream: StreamHandle = Default::default();
    {
        let mut s = stream.lock().unwrap();
        s.state = State::Ready;
        s.width = 16;
        s.height = 8;
        s.pixels = (0..256)
            .map(|i| ((i * 73 + i / 7) % 256) as u8)
            .collect::<Vec<_>>()
            .into();
    }
    let mut h = TestHost::new();
    h.ram[0x9000..0xa000].fill(0xa5);
    let mut e = encoder(stream.clone());
    configure(&mut e, &mut h, true);
    write(&mut e, &mut h, 0xfff62110, 640);
    write(&mut e, &mut h, 0xfff62000, 5);
    let saved = e.checkpoint().unwrap();
    e.restore_checkpoint(&saved).unwrap();
    assert!(h.effects.is_empty());
    h.now = 10;
    {
        let mut cx = h.cx();
        e.advance(&mut cx);
        assert_eq!(cx.refusal(), None);
    }
    // Stock channel8_complete: no READY bit means status 2, zero bytes.
    assert_eq!(read(&mut e, &mut h, 0xfff62024) & 0x30, 0x20);
    assert_eq!(read(&mut e, &mut h, 0xfff62094), 0);
    assert_eq!(stream.lock().unwrap().state, State::Finished);
    assert!(h.ram[0x9000..0xa000].iter().all(|&v| v == 0xa5));
    assert_eq!(h.irqs, [(0, 77)]);
    assert_eq!(e.next_deadline(10), None);
    fujimod_emulator_devices::work::configure(0).unwrap();
}
