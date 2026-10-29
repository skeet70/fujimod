//! FFF63000 channel9 JPEG decode, reset/ACK and IRQ76. The decoder is
//! zune-jpeg with SIMD features disabled and unsafe code disabled in options.
//! RGB box downsampling and full-range packed422 conversion are declared;
//! Playback output follows these register-selected decoding steps.
use fujimod_emulator_imaging::exr2_isp::{Irq, dma::{Raster, Transfer, physical}, due, word};
use fujimod_emulator_devices::{Cx, Device};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Options {
    pub completion_ticks: u64,
    pub reset_ticks: u64,
}
/// Bytes one decode writes from 0x100, bounded by the 0x104 capacity.
#[derive(Clone, Copy)]
struct Extent(u32);
impl Extent {
    fn within(bytes: usize, capacity: u32) -> Option<Self> {
        u32::try_from(bytes)
            .ok()
            .filter(|&n| n <= capacity)
            .map(Self)
    }
}
/// 0x108 after completion: raw 0x100, alias bits included, plus the written extent modulo 2^32.
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DestinationCursor {
    endpoint: u32,
}
impl DestinationCursor {
    fn after(destination: u32, written: Extent) -> Self {
        Self {
            endpoint: destination.wrapping_add(written.0),
        }
    }
}
#[derive(Clone, Copy, Serialize, Deserialize)]
enum Pending {
    Reset(u64),
    Clear(u64),
    Decode(DestinationCursor),
}
pub struct Decoder {
    r: Vec<u32>,
    options: Options,
    irq: Irq,
    source: Arc<[u8]>,
    dma: Transfer,
    pending: Option<Pending>,
}

#[inline]
fn rgb_pixel(bytes: &[u8], width: usize, factor: usize, x: usize, y: usize) -> [i32; 3] {
    if factor == 1 {
        let at = (y * width + x) * 3;
        return std::array::from_fn(|c| i32::from(bytes[at + c]));
    }
    let mut sums = [0i32; 3];
    for yy in y * factor..(y + 1) * factor {
        for xx in x * factor..(x + 1) * factor {
            let p = &bytes[(yy * width + xx) * 3..][..3];
            for c in 0..3 {
                sums[c] += i32::from(p[c]);
            }
        }
    }
    let n = (factor * factor) as i32;
    sums.map(|v| (v + n / 2) / n)
}

#[inline]
fn rgb_to_ycc([r, g, b]: [i32; 3]) -> [u8; 3] {
    // RGB and rounded box means are 0..=255. The largest intermediate is
    // 16,744,448, so signed 32-bit arithmetic preserves the integer law.
    [
        ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16).clamp(0, 255) as u8,
        (((-11059 * r - 21709 * g + 32768 * b + 32768) >> 16) + 128).clamp(0, 255) as u8,
        (((32768 * r - 27439 * g - 5329 * b + 32768) >> 16) + 128).clamp(0, 255) as u8,
    ]
}
impl Decoder {
    pub fn new(options: Options, irq: Irq) -> Result<Self, String> {
        if options.completion_ticks == 0 || options.reset_ticks == 0 {
            return Err("JPEG decoder timing must be positive".into());
        }
        Ok(Self {
            r: vec![0; 0x12c / 4],
            options,
            irq,
            source: Arc::from([]),
            dma: Transfer::default(),
            pending: None,
        })
    }
    fn valid(o: usize) -> bool {
        matches!(
            o,
            0 | 4 | 8 | 0x10 | 0x14 | 0x18 | 0x1c | 0xa0 | 0x100..=0x128
        )
    }
    fn start(&mut self, cx: &Cx<'_>) -> Result<(), String> {
        let r = &self.r;
        let (src, dst) = (
            (r[0x10c / 4] & 0x3fffffff) as u64,
            (r[0x100 / 4] & 0x3fffffff) as u64,
        );
        let (count, capacity) = (r[0x110 / 4] as usize, r[0x104 / 4] as usize);
        let destination = r[0x100 / 4];
        if r[2] != 0xfd0020
            || r[0xa0 / 4] > 3
            || [0x11c, 0x120, 0x124, 0x128].iter().any(|&o| r[o / 4] != 0)
        {
            return Err("JPEG decode register configuration is unsupported".into());
        }
        physical(src, count)?;
        physical(dst, capacity)?;
        self.source = vec![0; count].into();
        cx.ram_read(src, Arc::get_mut(&mut self.source).unwrap())?;
        if !self.source.starts_with(&[255, 0xd8]) {
            if count > 4
                && self.source[2] == 255
                && matches!(self.source[3], 0xc4 | 0xdb | 0xe0..=0xef)
            {
                Arc::get_mut(&mut self.source).unwrap()[..2].copy_from_slice(&[255, 0xd8]);
            } else {
                return Err("JPEG decode input has no supported stream header".into());
            }
        }
        let options = DecoderOptions::new_safe()
            .set_use_unsafe(false)
            .set_strict_mode(true)
            .set_max_width(8192)
            .set_max_height(8192)
            .jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut decoder =
            zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(self.source.as_ref()), options);
        decoder
            .decode_headers()
            .map_err(|e| format!("invalid JPEG header: {e:?}"))?;
        let info = decoder.info().ok_or("JPEG has no frame")?;
        let (sw, sh) = (info.width as usize, info.height as usize);
        let shift = r[0xa0 / 4] as usize;
        let (w, h) = (sw >> shift, sh >> shift);
        if sw * sh > 32_000_000 || w < 2 || w % 2 != 0 || h == 0 {
            return Err("JPEG decode dimensions are unsupported".into());
        }
        let pitch = (w + 15) & !15;
        let rows = (h + 7) & !7;
        let row_bytes = pitch * 2;
        let written = row_bytes * rows;
        let Some(extent) = Extent::within(written, r[0x104 / 4])
            .filter(|_| src >= dst + written as u64 || dst >= src + count as u64)
        else {
            return Err("JPEG output exceeds allocation or overlaps input".into());
        };
        let cursor = DestinationCursor::after(destination, extent);
        let size = decoder
            .output_buffer_size()
            .ok_or("JPEG output size unknown")?;
        self.dma.prepare_deferred(
            cx.now(),
            self.options.completion_ticks,
            Raster {
                address: dst,
                width: pitch,
                height: rows,
                pitch: row_bytes,
            },
            row_bytes,
        )?;
        let source = self.source.clone();
        self.dma.pixels.defer_fallible(move |mut pixels| {
            pixels.resize(written, 0);
            let result = (|| {
                let options = DecoderOptions::new_safe()
                    .set_use_unsafe(false)
                    .set_strict_mode(true)
                    .set_max_width(8192)
                    .set_max_height(8192)
                    .jpeg_set_out_colorspace(ColorSpace::RGB);
                let mut decoder = zune_jpeg::JpegDecoder::new_with_options(
                    ZCursor::new(source.as_ref()),
                    options,
                );
                let mut rgb = vec![0; size];
                decoder
                    .decode_into(&mut rgb)
                    .map_err(|e| format!("invalid JPEG image: {e:?}"))?;
                let factor = 1usize << shift;
                for y in 0..rows {
                    if y >= h {
                        continue;
                    }
                    for x in (0..w).step_by(2) {
                        let a = rgb_to_ycc(rgb_pixel(&rgb, sw, factor, x, y));
                        let b = rgb_to_ycc(rgb_pixel(&rgb, sw, factor, x + 1, y));
                        let at = (y * pitch + x) * 2;
                        pixels[at..at + 4].copy_from_slice(&[
                            a[0],
                            b[0],
                            (a[1] as u16 + b[1] as u16).div_ceil(2) as u8,
                            (a[2] as u16 + b[2] as u16).div_ceil(2) as u8,
                        ]);
                    }
                }
                Ok(())
            })();
            (pixels, result)
        });
        if fujimod_emulator_devices::work::worker_count() == 0
            && let Err(error) = self.dma.check_result()
        {
            self.dma.cancel();
            return Err(error);
        }
        self.pending = Some(Pending::Decode(cursor));
        Ok(())
    }
    fn store(&mut self, cx: &Cx<'_>, o: usize, v: u32) -> Result<(), String> {
        if [4, 0x10, 0x14].contains(&o) {
            self.r[o / 4] &= !v;
            return Ok(());
        }
        if self.pending.is_some() {
            return Err("JPEG decoder write while busy".into());
        }
        match (o, v) {
            (0x118, 1) => {
                self.pending = Some(Pending::Reset(due(cx.now(), self.options.reset_ticks)?))
            }
            (0, 0x100) => {
                self.pending = Some(Pending::Clear(due(cx.now(), self.options.reset_ticks)?))
            }
            (0, 1) => self.start(cx)?,
            (8 | 0xa0 | 0x100 | 0x104 | 0x10c | 0x110 | 0x11c | 0x120 | 0x124 | 0x128, _) => {}
            _ => return Err("unsupported JPEG decoder register write".into()),
        }
        self.r[o / 4] = v;
        Ok(())
    }
}
impl Device for Decoder {
    fujimod_emulator_devices::checkpoint_fields!(
        r: Vec<u32>,
        dma: Transfer,
        pending: Option<Pending>,
        ; blobs: [source]
    );
    fn fingerprint(&self) -> std::result::Result<serde_json::Value, String> {
        Ok(self.save())
    }

    fn name(&self) -> &'static str {
        "exr2-jpeg-decoder"
    }
    fn read(&mut self, cx: &mut Cx<'_>, a: u64, s: u8) -> u64 {
        let Some(o) = word(cx, a, s, 0xfff63000, 0x12c) else {
            return 0;
        };
        if !Self::valid(o) {
            cx.unmodeled();
            return 0;
        }
        self.r[o / 4] as u64
    }
    fn write(&mut self, cx: &mut Cx<'_>, a: u64, s: u8, v: u64) {
        let Some(o) = word(cx, a, s, 0xfff63000, 0x12c) else {
            return;
        };
        if !Self::valid(o) || v > u32::MAX as u64 {
            cx.unmodeled();
            return;
        }
        if let Err(e) = self.store(cx, o, v as u32) {
            cx.fail(e);
        }
    }
    fn next_deadline(&self, _: u64) -> Option<u64> {
        match self.pending {
            Some(Pending::Reset(t) | Pending::Clear(t)) => Some(t),
            Some(Pending::Decode(_)) => self.dma.deadline(),
            None => None,
        }
    }
    fn advance(&mut self, cx: &mut Cx<'_>) {
        if self.next_deadline(cx.now()).is_none_or(|t| cx.now() < t) {
            return;
        }
        match self.pending.unwrap() {
            Pending::Reset(_) => {
                self.r[1] |= 0x10000;
                self.r[0x118 / 4] = 0;
            }
            Pending::Clear(_) => self.r[0] = 0,
            Pending::Decode(cursor) => {
                match self.dma.write(cx) {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(e) => {
                        cx.fail(e);
                        return;
                    }
                }
                self.r[0x108 / 4] = cursor.endpoint;
                self.r[1] |= 0x10;
                Transfer::interrupt(cx, self.irq);
            }
        }
        self.pending = None;
    }
    fn event(&mut self, cx: &mut Cx<'_>, _: u64) {
        self.advance(cx)
    }
    fn save(&self) -> serde_json::Value {
        serde_json::json!((&self.r, &self.dma, &self.pending))
    }
    fn restore(&mut self, v: &serde_json::Value) -> Result<(), String> {
        let (r, d, p): (Vec<u32>, Transfer, Option<Pending>) =
            serde_json::from_value(v.clone()).map_err(|e| e.to_string())?;
        if r.len() != self.r.len() {
            return Err("JPEG decoder bank size".into());
        }
        self.r = r;
        self.dma = d;
        self.pending = p;
        Ok(())
    }
}

#[cfg(test)]
mod pixel_tests {
    use super::*;

    #[test]
    fn box_scaling_and_full_size_fast_path_preserve_i64_reference_colors() {
        let (width, height) = (64, 32);
        let bytes: Vec<_> = (0..width * height * 3)
            .map(|i| ((i * 73 + i / 17) % 256) as u8)
            .collect();
        for factor in [1, 2, 4, 8] {
            for y in 0..height / factor {
                for x in 0..width / factor {
                    let expected: [i64; 3] = std::array::from_fn(|c| {
                        let sum: i64 = (y * factor..(y + 1) * factor)
                            .flat_map(|yy| (x * factor..(x + 1) * factor).map(move |xx| (yy, xx)))
                            .map(|(yy, xx)| i64::from(bytes[(yy * width + xx) * 3 + c]))
                            .sum();
                        let n = (factor * factor) as i64;
                        (sum + n / 2) / n
                    });
                    assert_eq!(
                        rgb_pixel(&bytes, width, factor, x, y).map(i64::from),
                        expected
                    );
                }
            }
        }
    }

    #[test]
    fn narrowed_color_conversion_preserves_i64_rounding_and_clipping() {
        let reference = |[r, g, b]: [i64; 3]| {
            [
                ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16).clamp(0, 255) as u8,
                (((-11059 * r - 21709 * g + 32768 * b + 32768) >> 16) + 128).clamp(0, 255) as u8,
                (((32768 * r - 27439 * g - 5329 * b + 32768) >> 16) + 128).clamp(0, 255) as u8,
            ]
        };
        for r in [0, 1, 127, 128, 254, 255] {
            for g in [0, 1, 127, 128, 254, 255] {
                for b in [0, 1, 127, 128, 254, 255] {
                    assert_eq!(rgb_to_ycc([r, g, b]), reference([r, g, b].map(i64::from)));
                }
            }
        }
        let mut state = 7u64;
        for _ in 0..65_536 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let rgb = [state as u8, (state >> 16) as u8, (state >> 32) as u8].map(i32::from);
            assert_eq!(rgb_to_ycc(rgb), reference(rgb.map(i64::from)));
        }
    }
}
