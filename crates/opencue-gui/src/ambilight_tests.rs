//! Exercise the actual Slint pointer handling: model updates during a drag must preserve the
//! pointer's grab offset and must not make a stationary pointer move the device again.

use super::*;
use slint::platform::{PointerEventButton, WindowAdapter, WindowEvent};

#[test]
fn device_markers_drag_stably_and_stay_on_the_monitor() {
    let window = screenshot::install().unwrap();
    let ui = AppWindow::new().unwrap();
    ui.set_page(2);
    ui.set_connected(true);
    ui.set_zones(ModelRc::new(VecModel::from(vec![Zone { id: ss("*"), name: ss("All devices") }])));
    ui.set_effect_names(strings(["Ambilight"]));
    ui.set_uses_ambilight(true);
    ui.set_ambilight_size(20.0);
    ui.set_brightness(100.0);
    let tint = Color::from_rgb_u8(211, 47, 139);
    let devices = Rc::new(VecModel::from(vec![AmbilightDevice {
        id: ss("test"),
        name: ss("Test device"),
        px: 0.1,
        py: 0.2,
        tint,
        enabled: true,
    }]));
    ui.set_ambilight_devices(ModelRc::from(devices.clone()));
    ui.on_ambilight_moved({
        let devices = devices.clone();
        move |id, x, y| {
            assert_eq!(id.as_str(), "test");
            let mut device = devices.row_data(0).unwrap();
            device.px = x.clamp(0.0, 1.0);
            device.py = y.clamp(0.0, 1.0);
            devices.set_row_data(0, device);
        }
    });
    ui.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); (screenshot::WIDTH * screenshot::HEIGHT) as usize];
    let mut draw = || {
        for _ in 0..4 {
            slint::platform::update_timers_and_animations();
            window.request_redraw();
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, screenshot::WIDTH as usize);
            });
        }
        pixels.clone()
    };
    // Locate the marker by its unique border colour, so the test tolerates layout changes.
    let rendered = draw();
    let border: Vec<_> = rendered
        .iter()
        .enumerate()
        .filter(|(_, p)| [p.r, p.g, p.b] == [211, 47, 139])
        .map(|(i, _)| ((i % screenshot::WIDTH as usize) as f32, (i / screenshot::WIDTH as usize) as f32))
        .collect();
    assert!(!border.is_empty(), "device marker must be visible");
    let x = (border.iter().map(|p| p.0).fold(f32::INFINITY, f32::min) + border.iter().map(|p| p.0).fold(0.0, f32::max))
        / 2.0;
    let y = (border.iter().map(|p| p.1).fold(f32::INFINITY, f32::min) + border.iter().map(|p| p.1).fold(0.0, f32::max))
        / 2.0;
    let position = slint::LogicalPosition::new(x, y);
    ui.window().dispatch_event(WindowEvent::PointerMoved { position });
    ui.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
    let position = slint::LogicalPosition::new(x + 100.0, y + 35.0);
    ui.window().dispatch_event(WindowEvent::PointerMoved { position });
    draw();
    let moved = devices.row_data(0).unwrap();
    assert!(moved.px > 0.1 && moved.py > 0.2, "drag must follow the pointer: {moved:?}");
    ui.window().dispatch_event(WindowEvent::PointerMoved { position });
    draw();
    let stationary = devices.row_data(0).unwrap();
    assert!((moved.px - stationary.px).abs() < 0.0001);
    assert!((moved.py - stationary.py).abs() < 0.0001);

    ui.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(-500.0, -500.0) });
    draw();
    let corner = devices.row_data(0).unwrap();
    assert_eq!((corner.px, corner.py), (0.0, 0.0));
    let position = slint::LogicalPosition::new(2000.0, 1500.0);
    ui.window().dispatch_event(WindowEvent::PointerMoved { position });
    draw();
    let corner = devices.row_data(0).unwrap();
    assert_eq!((corner.px, corner.py), (1.0, 1.0));
    ui.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
}
