// aris_browser — a browser that renders web content in a winit window.
//
// Features:
//   - Browser chrome: address bar, back/forward/reload buttons
//   - Navigation: clicking <a href>, form submission, address-bar URL entry
//   - Networking: HTTP(S) and file:// fetching of pages + subresources
//   - Text input: typing into <input>/<textarea>
//   - HiDPI super-sampled rendering
//
// Usage:
//   aris_browser                 # start page (new-tab bookmarks)
//   aris_browser page.html       # load local HTML file
//   aris_browser ./docs          # load local directory index (file://)
//   aris_browser https://example.com   # fetch and render a web page
//   aris_browser "search terms"  # web search
//
// Keyboard:
//   Ctrl+L  focus the address bar       Ctrl+R / F5  reload
//   Enter   (in address bar) navigate   Esc          blur address bar / quit
//   Alt+Left/Right  back/forward (via address bar typing not yet)

fn main() {
    aris_render::init_logging();
    let arg = std::env::args().nth(1);
    let config = aris_render::RenderConfig {
        width: 1024,
        height: 768,
        scale: 1.0,
    };

    if let Some(target) = arg {
        tracing::info!("loading {}", target);
        if let Err(e) = aris_render::winit_backend::run_window_url(&target, &config) {
            tracing::error!("error: {:?}", e);
            std::process::exit(1);
        }
    } else {
        // No argument: show the start page.
        tracing::info!("opening start page ({}x{})...", config.width, config.height);
        let html = start_page();
        if let Err(e) = aris_render::winit_backend::run_window(&html, &config) {
            tracing::error!("error: {:?}", e);
            std::process::exit(1);
        }
    }
}

fn start_page() -> String {
    "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\">\
     <title>aris — new tab</title>\
     <style>\
     body { margin:0; background:#1a1b26; font-family:system-ui,sans-serif; color:#a9b1d6; }\
     .wrap { max-width:640px; margin:80px auto; padding:0 24px; text-align:center; }\
     h1 { color:#7aa2f7; font-size:42px; margin:0 0 8px; }\
     p.sub { color:#565f89; margin:0 0 32px; }\
     .bookmarks { display:grid; grid-template-columns:repeat(2,1fr); gap:12px; }\
     a.card { display:block; background:#24283b; color:#c0caf5; text-decoration:none;\
              padding:16px; border-radius:10px; transition:background .15s; }\
     a.card:hover { background:#2f344d; }\
     .card .t { color:#7dcfff; font-weight:600; }\
     .card .d { color:#565f89; font-size:13px; margin-top:4px; }\
     .hint { color:#565f89; font-size:13px; margin-top:32px; }\
     kbd { background:#24283b; padding:2px 6px; border-radius:4px; color:#7dcfff; }\
     </style></head><body>\
     <div class=\"wrap\">\
       <h1>aris</h1>\
       <p class=\"sub\">a browser engine built on servo's pure-Rust front-end</p>\
       <div class=\"bookmarks\">\
         <a class=\"card\" href=\"https://example.com\"><span class=\"t\">example.com</span><div class=\"d\">test page</div></a>\
         <a class=\"card\" href=\"about:about\"><span class=\"t\">about:about</span><div class=\"d\">aris info</div></a>\
       </div>\
       <p class=\"hint\">Type a URL or search in the address bar, or press <kbd>Ctrl+L</kbd>.</p>\
     </div>\
     </body></html>"
        .to_string()
}
