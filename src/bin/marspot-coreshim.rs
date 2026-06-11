//! marspot-coreshim — temporary core stub for Step 1.
//!
//! Reads the IOSurface ID, width, and height from environment variables
//! the shell sets, attaches to the shared surface, and draws a rotating
//! gradient via a Metal render pass.  No event loop, no parser, no PTY
//! — its only job is to prove the cross-process IOSurface link works
//! before we point the real core at it in Step 2.
//!
//! Behaviour: 60 fps, exits when the parent shell process exits
//! (writes to the shared surface and Metal device release naturally
//! on Drop; nothing else to clean up).

use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLoadAction, MTLRenderPassDescriptor, MTLStoreAction,
};

use marspot::iosurface::IOSurface;
use marspot::shell_proto::{
    ENV_SURFACE_HEIGHT, ENV_SURFACE_ID, ENV_SURFACE_SCALE, ENV_SURFACE_WIDTH,
};

const FRAME_MS: u64 = 16;

fn env_required<T: std::str::FromStr>(name: &str) -> T {
    let raw = std::env::var(name)
        .unwrap_or_else(|_| panic!("[coreshim] missing required env var {name}"));
    raw.parse::<T>()
        .ok()
        .unwrap_or_else(|| panic!("[coreshim] env {name} = {raw:?} failed to parse"))
}

fn main() {
    let surface_id: u32 = env_required(ENV_SURFACE_ID);
    let w: usize = env_required(ENV_SURFACE_WIDTH);
    let h: usize = env_required(ENV_SURFACE_HEIGHT);
    let scale: f64 = env_required(ENV_SURFACE_SCALE);

    eprintln!(
        "[coreshim] attaching surface_id={surface_id} w={w} h={h} scale={scale} pid={}",
        std::process::id()
    );

    let surface = IOSurface::lookup(surface_id)
        .unwrap_or_else(|| panic!("[coreshim] IOSurfaceLookup({surface_id}) returned nil"));
    surface.increment_use();

    let device = system_default_device();
    let queue = device
        .newCommandQueue()
        .expect("[coreshim] newCommandQueue returned nil");
    let tex = surface
        .make_metal_texture(&device)
        .expect("[coreshim] make_metal_texture failed");

    eprintln!(
        "[coreshim] ready: device + texture wired; rendering at ~{} fps",
        1000 / FRAME_MS
    );

    let start = Instant::now();
    let mut frame: u64 = 0;
    loop {
        let frame_start = Instant::now();
        let t = start.elapsed().as_secs_f64();
        // Rotating RGB at ~6 sec period — easy visual proof the IOSurface
        // is being shared correctly.  Cycle through pure hues so any
        // pipeline-format mismatch (BGRA vs RGBA) shows up as a colour
        // swap rather than going unnoticed.
        let r = (0.5 + 0.5 * (t * 1.0).sin()).clamp(0.0, 1.0);
        let g = (0.5 + 0.5 * (t * 1.3 + 2.0).sin()).clamp(0.0, 1.0);
        let b = (0.5 + 0.5 * (t * 0.7 + 4.0).sin()).clamp(0.0, 1.0);

        let pass = unsafe { MTLRenderPassDescriptor::new() };
        let color0 = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        color0.setTexture(Some(&tex));
        color0.setLoadAction(MTLLoadAction::Clear);
        color0.setStoreAction(MTLStoreAction::Store);
        color0.setClearColor(MTLClearColor {
            red: r,
            green: g,
            blue: b,
            alpha: 1.0,
        });

        let cmd = queue.commandBuffer().expect("[coreshim] commandBuffer nil");
        let encoder = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("[coreshim] renderCommandEncoder nil");
        encoder.endEncoding();
        cmd.commit();
        unsafe { cmd.waitUntilCompleted() };

        frame += 1;
        if frame.is_multiple_of(60) {
            eprintln!("[coreshim] frame {frame} t={t:.2}s rgb=({r:.2},{g:.2},{b:.2})");
        }

        let elapsed = frame_start.elapsed();
        let target = Duration::from_millis(FRAME_MS);
        if elapsed < target {
            std::thread::sleep(target - elapsed);
        }
    }
}

fn system_default_device() -> Retained<ProtocolObject<dyn MTLDevice>> {
    // Mirror render_metal's pattern: MTLCreateSystemDefaultDevice
    // returns a +1 retained raw pointer.
    let raw = unsafe { MTLCreateSystemDefaultDevice() };
    if raw.is_null() {
        panic!("[coreshim] MTLCreateSystemDefaultDevice returned nil");
    }
    unsafe { Retained::from_raw(raw) }
        .expect("[coreshim] non-null then null on retain — impossible")
}
