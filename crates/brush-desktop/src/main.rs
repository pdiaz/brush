fn main() -> anyhow::Result<()> {
    let wgpu_options = brush_viewer::wgpu_config::get_config();

    #[cfg(not(target_arch = "wasm32"))]
    {
        // Build app display.
        let native_options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size(egui::Vec2::new(1280.0, 720.0))
                .with_active(true),
            // Need a slightly more careful wgpu init to support burn.
            wgpu_options,
            ..Default::default()
        };
        eframe::run_native(
            "Brush 🖌️",
            native_options,
            Box::new(move |cc| Ok(Box::new(brush_viewer::viewer::Viewer::new(cc)))),
        )
        .unwrap();
    }

    #[cfg(target_arch = "wasm32")]
    {
        console_error_panic_hook::set_once();

        let web_options = eframe::WebOptions {
            wgpu_options,
            ..Default::default()
        };

        wasm_bindgen_futures::spawn_local(async {
            eframe::WebRunner::new()
                .start(
                    "main_canvas", // hardcode it
                    web_options,
                    Box::new(|cc| Ok(Box::new(Viewer::new(cc)))),
                )
                .await
                .expect("failed to start eframe");
        });
    }

    Ok(())
}
