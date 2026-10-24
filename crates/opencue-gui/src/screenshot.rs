//! Off-screen rendering for `opencue-gui --screenshot <page> <file.png>`: draws the window with Slint's
//! software renderer into a PNG, without opening anything on the desktop.

use std::rc::Rc;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, Rgb8Pixel};

pub const WIDTH: u32 = 1180;
pub const HEIGHT: u32 = 760;

struct OffscreenPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for OffscreenPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

/// Must run before the first component is created.
pub fn install() -> anyhow::Result<Rc<MinimalSoftwareWindow>> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(OffscreenPlatform(window.clone())))
        .map_err(|e| anyhow::anyhow!("cannot set up off-screen rendering: {e}"))?;
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
    Ok(window)
}

pub fn capture(ui: &impl ComponentHandle, window: &MinimalSoftwareWindow, out: &str) -> anyhow::Result<()> {
    ui.show()?;
    let mut pixels = vec![Rgb8Pixel::default(); (WIDTH * HEIGHT) as usize];
    // A few passes: layout-dependent callbacks (like the curve editor's size) update state after the first draw.
    for _ in 0..4 {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, WIDTH as usize);
        });
    }
    let raw: Vec<u8> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
    image::RgbImage::from_raw(WIDTH, HEIGHT, raw).expect("buffer size matches").save(out)?;
    println!("wrote {out}");
    Ok(())
}
