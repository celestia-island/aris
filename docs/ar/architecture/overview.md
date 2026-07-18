# بنية ARIS

## نظرة عامة

ARIS هو محرك متصفح مبني على servo. يمكن تضمينه كمكتبة أو تشغيله كمتصفح مستقل. يستخدم خط أنابيب التصيير صناديق Rust خالصة، مستبدلاً SpiderMonkey / WebRender / components/script من servo بـ Boa / Vello CPU / Wasmtime.

## الاستبدالات الرئيسية

| مكون Servo | بديل ARIS | السبب |
|-----------|----------|------|
| SpiderMonkey (C++) | boa_engine | Rust خالص، بدون بناء C++ |
| WebRender + SWGL (C++) | vello_cpu | تنقيط CPU بـ Rust خالص |
| components/script | جسر Boa | بدون اقتران بـ SpiderMonkey |
| — | wasmtime | WASM Component Model + WASI |

## خلفيات العرض

| الخلفية | الاستخدام |
|---------|----------|
| /dev/fb0 mmap | الأجهزة المضمنة، نواة kei |
| winit + softbuffer | سطح المكتب (Linux/macOS/Windows) |
| WASM canvas | التضمين في المتصفح (WASM) |

## وضعان للتشغيل

1. **الوضع المضمن** (مكتبة): `render_html()` تنتج مخزن بكسل مؤقت
2. **الوضع المستقل** (متصفح): `render_window` يفتح نافذة كاملة


