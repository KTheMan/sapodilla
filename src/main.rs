#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    tracing_subscriber::fmt::init();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Sapodilla",
        native_options,
        Box::new(|cc| Ok(Box::new(sapodilla::SapodillaApp::new(cc)))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;

    // Install this for both the UI and isolated computation workers so a Rust
    // panic is reported with its message instead of only `RuntimeError:
    // unreachable`.
    console_error_panic_hook::set_once();

    // The calibration worker initializes this same WebAssembly module with a
    // separate linear memory. It needs the exported calibration functions,
    // not a second eframe application (Workers have no Window or canvas).
    if web_sys::window().is_none() {
        return;
    }

    wasm_tracing::set_as_global_default();

    let web_options = eframe::WebOptions::default();

    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window()
            .expect("No window")
            .document()
            .expect("No document");

        let canvas = document
            .get_element_by_id("sapodilla_canvas")
            .expect("Failed to find sapodilla_canvas")
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("sapodilla_canvas was not a HtmlCanvasElement");

        let start_result = eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| Ok(Box::new(sapodilla::SapodillaApp::new(cc)))),
            )
            .await;

        // Remove the loading text and spinner:
        if let Some(loading_text) = document.get_element_by_id("loading_text") {
            match start_result {
                Ok(_) => {
                    loading_text.remove();
                }
                Err(e) => {
                    loading_text.set_inner_html(
                        "<p> The app has crashed. See the developer console for details. </p>",
                    );
                    panic!("Failed to start eframe: {e:?}");
                }
            }
        }
    });
}
