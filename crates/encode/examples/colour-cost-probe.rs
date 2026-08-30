//! What full-resolution chroma costs on the vendor encoder.
//!
//!   colour-cost-probe [frames] [mbit] [fps]
//!
//! **One question, asked because the only figure anyone has for it is from
//! another platform and does not transfer.** The Windows measurement that
//! gets quoted -- roughly 7 ms against 2 -- was taken on a path where 4:4:4
//! also loses encode overlap, because that encoder reads the capture's own
//! staging texture and the next acquire may not overlap the read. Nothing
//! like that applies here: the overlap comes from a per-slot ring and not
//! from the input format. So the number has to be taken again, on this
//! platform, or it is not a number.
//!
//! **Serialized deliberately.** Each picture is submitted and then collected
//! before the next is written, so what is timed is one encode and not the
//! pipeline's throughput. Overlap would hide exactly the difference this is
//! looking for.
//!
//! **Same content both ways.** The two runs draw one picture and derive both
//! chroma layouts from it, so the comparison is the format rather than what
//! happened to be drawn. 4:2:0 averages each 2x2 block the way the conversion
//! shader does; 4:4:4 keeps every sample.
//!
//! It reports the coded size as well as the time, because bandwidth is the
//! other half of the cost and the wire is the scarcer resource of the two.

use lowlat_encode::cuda;
use lowlat_encode::nvenc;

fn fail(why: &str) -> ! {
    eprintln!("{why}");
    std::process::exit(2);
}

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;

/// One frame's worth of colour, as the three planes at full resolution.
///
/// Kept full-resolution for both layouts so the subsampling happens in one
/// place below and the two runs cannot drift apart in what they encode.
struct Planes {
    luma: Vec<u8>,
    cb: Vec<u8>,
    cr: Vec<u8>,
}

impl Planes {
    fn new() -> Self {
        let pixels = (WIDTH as usize) * (HEIGHT as usize);
        Self {
            luma: vec![0; pixels],
            cb: vec![0; pixels],
            cr: vec![0; pixels],
        }
    }

    /// Low-frequency structure that moves, which is what ordinary desktop
    /// content is. Noise would pin any encoder at its coarsest quantiser,
    /// where a size comparison measures the content rather than the format.
    fn draw(&mut self, at: u32) {
        // BT.709 limited range, the same constants the conversion shader
        // carries. Written out here rather than shared because a probe that
        // imports the thing it is measuring can agree with it and still be
        // wrong about the hardware.
        const KR: f32 = 0.2126;
        const KB: f32 = 0.0722;
        const KG: f32 = 1.0 - KR - KB;

        let phase = at.wrapping_mul(3);
        let bar = (phase / 2) % WIDTH;
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let dx = x.abs_diff(bar);
                let mut r = ((x.wrapping_add(phase) / 8) & 0xff) as f32 / 255.0;
                let mut g = ((y / 8) & 0xff) as f32 / 255.0;
                let mut b = (200_u32.saturating_sub(dx) & 0xff) as f32 / 255.0;

                // **A third of the picture carries chroma detail at the pixel,
                // because that is the only thing 4:4:4 buys.** Smooth colour
                // subsamples for free and would measure this format as costing
                // nothing, which is true and useless: what a desktop actually
                // holds is coloured text, and to a codec that is thin vertical
                // strokes whose colour changes every pixel or two. Luma stays
                // put across the strokes so the difference lands where 4:2:0
                // is lossy and nowhere else.
                if y % 3 == 0 && (x / 2) % 2 == 0 {
                    let tint = (x / 2).wrapping_add(y / 3).wrapping_add(phase / 4) % 3;
                    r = if tint == 0 { 0.9 } else { 0.1 };
                    g = if tint == 1 { 0.9 } else { 0.1 };
                    b = if tint == 2 { 0.9 } else { 0.1 };
                }

                let luma = KR * r + KG * g + KB * b;
                let u = (b - luma) / (2.0 - 2.0 * KB);
                let v = (r - luma) / (2.0 - 2.0 * KR);

                let at = (y as usize) * (WIDTH as usize) + (x as usize);
                self.luma[at] = quantise(luma * (219.0 / 255.0) + 16.0 / 255.0);
                self.cb[at] = quantise(u * (224.0 / 255.0) + 128.0 / 255.0);
                self.cr[at] = quantise(v * (224.0 / 255.0) + 128.0 / 255.0);
            }
        }
    }

    /// Luma, then half as many rows of interleaved colour averaged over each
    /// 2x2 block -- the layout the vendor interface calls NV12.
    fn as_nv12(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.luma);
        let width = WIDTH as usize;
        for y in (0..HEIGHT as usize).step_by(2) {
            for x in (0..width).step_by(2) {
                out.push(mean_2x2(&self.cb, x, y));
                out.push(mean_2x2(&self.cr, x, y));
            }
        }
    }

    /// Three full planes, which is what the interface calls YUV444.
    fn as_yuv444(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(&self.luma);
        out.extend_from_slice(&self.cb);
        out.extend_from_slice(&self.cr);
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to the byte range on the line before the cast"
)]
fn quantise(value: f32) -> u8 {
    (value * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

fn mean_2x2(plane: &[u8], x: usize, y: usize) -> u8 {
    let width = WIDTH as usize;
    let right = (x + 1).min(width - 1);
    let below = (y + 1).min(HEIGHT as usize - 1);
    let sum = u32::from(plane[y * width + x])
        + u32::from(plane[y * width + right])
        + u32::from(plane[below * width + x])
        + u32::from(plane[below * width + right]);
    u8::try_from(sum / 4).unwrap_or(0)
}

/// What one run came to.
struct Run {
    /// Per-picture encode, sorted, in milliseconds.
    times: Vec<f64>,
    bytes: u64,
    frames: u32,
}

impl Run {
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "an index into a sample count that fits a float exactly, and                   the result is clamped to the last element either way"
    )]
    fn percentile(&self, at: f64) -> f64 {
        if self.times.is_empty() {
            return 0.0;
        }
        let index = ((self.times.len() as f64 - 1.0) * at).round() as usize;
        self.times[index.min(self.times.len() - 1)]
    }

    fn mbps(&self, fps: u32) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }
        let per_frame = self.bytes as f64 / f64::from(self.frames);
        per_frame * 8.0 * f64::from(fps) / 1_000_000.0
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let frames: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(600);
    let mbit: f32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10.0);
    let fps: u32 = args
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60);

    let cuda = cuda::Cuda::load().unwrap_or_else(|e| fail(&format!("compute runtime: {e}")));
    let device = cuda
        .any_device()
        .unwrap_or_else(|e| fail(&format!("no device: {e}")));
    let api = nvenc::Api::load().unwrap_or_else(|e| fail(&format!("encoder runtime: {e}")));

    // What the hardware says before anything is built on it. A refusal here
    // is the honest end of the probe, not a reason to try anyway.
    {
        let context = cuda
            .retain_primary(&device)
            .unwrap_or_else(|e| fail(&format!("context: {e}")));
        let session = api
            .open_session(context)
            .unwrap_or_else(|e| fail(&format!("session: {e}")));
        for codec in [nvenc::Codec::H264, nvenc::Codec::H265] {
            match session.caps(codec) {
                Ok(caps) => println!(
                    "{codec:?}: 4:4:4 {}   10-bit {}",
                    yes_no(caps.yuv444),
                    yes_no(caps.ten_bit)
                ),
                Err(error) => println!("{codec:?}: caps unavailable: {error}"),
            }
        }
    }

    println!("\n{WIDTH}x{HEIGHT}, {frames} frames, {mbit} Mbit/s asked, {fps} fps, serialized");
    let mut results = Vec::new();
    for chroma in [nvenc::Chroma::Yuv420, nvenc::Chroma::Yuv444] {
        let run = measure(&cuda, &device, &api, chroma, frames, mbit, fps);
        println!(
            "  {:<7} p50 {:>5.2} ms   p99 {:>5.2} ms   {:>6.2} Mbit/s at the asked rate",
            match chroma {
                nvenc::Chroma::Yuv420 => "4:2:0",
                nvenc::Chroma::Yuv444 => "4:4:4",
            },
            run.percentile(0.50),
            run.percentile(0.99),
            run.mbps(fps),
        );
        results.push(run);
    }

    if let (Some(base), Some(full)) = (results.first(), results.get(1)) {
        let p50 = base.percentile(0.50);
        let bits = base.mbps(fps);
        println!(
            "\n  4:4:4 costs {:.2} ms a picture ({:.2}x) and {:.2}x the bytes",
            full.percentile(0.50) - p50,
            if p50 > 0.0 {
                full.percentile(0.50) / p50
            } else {
                0.0
            },
            if bits > 0.0 {
                full.mbps(fps) / bits
            } else {
                0.0
            },
        );
    }
}

fn measure(
    cuda: &cuda::Cuda,
    device: &cuda::Device,
    api: &nvenc::Api,
    chroma: nvenc::Chroma,
    frames: u32,
    mbit: f32,
    fps: u32,
) -> Run {
    let context = cuda
        .retain_primary(device)
        .unwrap_or_else(|e| fail(&format!("context: {e}")));
    let session = api
        .open_session(context)
        .unwrap_or_else(|e| fail(&format!("session: {e}")));
    let config = nvenc::Config {
        codec: nvenc::Codec::H265,
        width: WIDTH,
        height: HEIGHT,
        fps,
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a bitrate in megabits, from the command line"
        )]
        bitrate_bps: (mbit * 1_000_000.0) as u32,
        min_qp: lowlat_encode::DEFAULT_MIN_QP,
        chroma,
    };
    let mut encoder = session
        .initialize(cuda, config)
        .unwrap_or_else(|e| fail(&format!("{chroma:?}: initialize: {e}")));

    // Our own picture rather than the pool's, because the pool is written
    // through a path that knows one layout and this probe needs both.
    let rows = match chroma {
        nvenc::Chroma::Yuv420 => HEIGHT as usize + (HEIGHT as usize).div_ceil(2),
        nvenc::Chroma::Yuv444 => HEIGHT as usize * 3,
    };
    let buffer = cuda
        .alloc_pitch(WIDTH as usize, rows)
        .unwrap_or_else(|e| fail(&format!("allocate: {e}")));
    let input = encoder
        .register(&buffer)
        .unwrap_or_else(|e| fail(&format!("register: {e}")));

    let mut dump = std::env::var_os("LOWLAT_DUMP").map(|dir| {
        let name = std::path::Path::new(&dir).join(match chroma {
            nvenc::Chroma::Yuv420 => "colour-cost-420.h265",
            nvenc::Chroma::Yuv444 => "colour-cost-444.h265",
        });
        std::fs::File::create(&name).unwrap_or_else(|e| fail(&format!("{}: {e}", name.display())))
    });

    let mut planes = Planes::new();
    let mut packed = Vec::new();
    let mut run = Run {
        times: Vec::with_capacity(frames as usize),
        bytes: 0,
        frames: 0,
    };

    for frame in 0..frames {
        planes.draw(frame);
        match chroma {
            nvenc::Chroma::Yuv420 => planes.as_nv12(&mut packed),
            nvenc::Chroma::Yuv444 => planes.as_yuv444(&mut packed),
        }
        buffer
            .write_rows(0, &packed, WIDTH as usize, WIDTH as usize, rows)
            .unwrap_or_else(|e| fail(&format!("upload: {e}")));

        // **Timed from the submit, not from the draw.** The content is host
        // work and identical in shape for both layouts apart from the copy,
        // which is the encoder's input bandwidth and belongs to neither.
        let started = std::time::Instant::now();
        encoder
            .submit_registered(&input, frame == 0)
            .unwrap_or_else(|e| fail(&format!("submit: {e}")));
        loop {
            match encoder.poll() {
                Ok(lowlat_encode::Poll::Ready { bitstream, .. }) => {
                    // The keyframe is excluded from both figures together, so
                    // the rate and the time describe the same pictures.
                    if frame > 0 {
                        run.bytes += bitstream.len() as u64;
                    }
                    // **Kept so an outside decoder can say what this really
                    // is.** Two runs that silently encoded the same chroma
                    // would produce a believable comparison of nothing, and
                    // nothing inside this process can tell the difference.
                    if let Some(dump) = dump.as_mut() {
                        use std::io::Write as _;
                        let _ = dump.write_all(bitstream);
                    }
                    break;
                }
                Ok(lowlat_encode::Poll::Pending) => {}
                Err(error) => fail(&format!("poll: {error}")),
            }
        }
        let took = started.elapsed().as_secs_f64() * 1000.0;

        // **The first picture is not a measurement.** It is a keyframe against
        // a cold encoder, and one of those in six hundred moves a p99.
        if frame > 0 {
            run.times.push(took);
            run.frames += 1;
        }
    }
    run.times.sort_by(f64::total_cmp);
    run
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}
