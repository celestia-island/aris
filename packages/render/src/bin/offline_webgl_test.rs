// Offline test for WebGL + WebRTC stubs.
//
// Runs a <script> that exercises WebGL (getContext, createShader, drawArrays)
// and WebRTC (new RTCPeerConnection, navigator.mediaDevices.getUserMedia).
// Verifies the script completes without crashing.
//
//   cargo run -p aris-render --features "desktop winit js" --bin offline_webgl_test

use std::sync::Arc;

use aris_render::browser::{BrowserNavigationProvider, BrowserShellProvider, HttpNetProvider};
use blitz_dom::DocumentConfig;
use blitz_html::HtmlDocument;
use blitz_traits::shell::Viewport;

fn main() {
    aris_render::init_logging();

    let html = r#"<!DOCTYPE html><html><head><meta charset="UTF-8"><title>webgl test</title>
<script>
  // WebGL: get a context, create a shader, "draw".
  var canvas = document.createElement('canvas');
  var gl = canvas.getContext('webgl');
  if (gl) {
    var shader = gl.createShader(gl.VERTEX_SHADER);
    gl.shaderSource(shader, 'void main(){}');
    gl.compileShader(shader);
    var program = gl.createProgram(); // wait, createProgram isn't in stubs... use noop check
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    var err = gl.getError();
    console.log('WebGL getError returned: ' + err);
  }

  // WebRTC: create a peer connection (should not crash).
  var pc = new RTCPeerConnection({});
  if (pc) {
    pc.createOffer();
  }

  // getUserMedia (should return undefined, not crash).
  navigator.mediaDevices.getUserMedia({video: true});

  // WebSocket stub.
  var ws = new WebSocket('ws://example.com');

  // Mark success.
  document.getElementById('out').setText('all stubs OK');
</script>
</head><body>
<div id="out">waiting</div>
</body></html>"#;

    let viewport = Viewport {
        window_size: (300, 150),
        hidpi_scale: 1.0,
        ..Default::default()
    };
    let state = Arc::new(aris_render::browser::BrowserState::new());
    let doc_config = DocumentConfig {
        viewport: Some(viewport),
        net_provider: Some(Arc::new(HttpNetProvider::new())),
        navigation_provider: Some(Arc::new(BrowserNavigationProvider::new(Arc::clone(&state)))),
        shell_provider: Some(Arc::new(BrowserShellProvider::new(Arc::clone(&state)))),
        ..Default::default()
    };

    let mut doc = HtmlDocument::from_html(html, doc_config);
    doc.resolve(0.0);

    let mut rt = aris_render::js_runtime::JsRuntime::new();
    let scripts = aris_js::extract_scripts(html);
    rt.bind_and_run(&mut doc, &scripts.join("\n;\n"));

    // Check the result.
    let out_id = doc
        .tree()
        .iter()
        .find(|(_, n)| n.attr(blitz_dom::local_name!("id")) == Some("out"))
        .map(|(id, _)| id);

    if let Some(oid) = out_id {
        let text = doc
            .get_node(oid)
            .map(|n| n.text_content())
            .unwrap_or_default();
        println!("#out = {:?}", text);
        if text.contains("all stubs OK") {
            println!("OK: WebGL + WebRTC stubs ran without crashing");
        } else {
            println!("FAIL: expected 'all stubs OK', got {:?}", text);
            std::process::exit(2);
        }
    } else {
        println!("FAIL: no #out element");
        std::process::exit(2);
    }
}
