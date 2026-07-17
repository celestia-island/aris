//! WebGL translation layer: maps WebGL API calls to wgpu commands.
//!
//! When `getContext('webgl')` is called on a canvas, this module creates a
//! real wgpu device (headless, using DX12/Vulkan/Metal or software WARP/
//! Lavapipe), tracks GL state, and renders to an offscreen texture. The
//! resulting RGBA pixels are then composited onto the page.
//!
//! Supported: createShader/shaderSource/compileShader (GLSL→WGSL via naga),
//! createProgram/linkProgram/useProgram, viewport, clearColor, clear,
//! drawArrays (triangles), drawingBufferWidth/Height, getError.
//! Uniforms, buffers, and textures are future work.

#![cfg(feature = "webgl")]

/// A WebGL context backed by a real wgpu device.
pub struct WebGLContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    width: u32,
    height: u32,
    /// The offscreen render target texture.
    target: Option<wgpu::Texture>,
    /// Compiled vertex shader source (WGSL).
    vertex_wgsl: Option<String>,
    /// Compiled fragment shader source (WGSL).
    fragment_wgsl: Option<String>,
    /// Whether a valid program is linked and in use.
    program_active: bool,
    /// Clear color.
    clear_color: [f64; 4],
    /// Latest GL error code (0 = NO_ERROR).
    error: u32,
}

impl WebGLContext {
    /// Create a new WebGL context with a headless wgpu device.
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: None,
            force_fallback_adapter: true,
            ..Default::default()
        }))
        .or_else(|_| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        })
        .ok()?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
        let mut ctx = Self {
            device,
            queue,
            width,
            height,
            target: None,
            vertex_wgsl: None,
            fragment_wgsl: None,
            program_active: false,
            clear_color: [0.0, 0.0, 0.0, 0.0],
            error: 0,
        };
        ctx.ensure_target();
        Some(ctx)
    }

    fn ensure_target(&mut self) {
        if self.target.is_none() {
            self.target = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("aris webgl target"),
                size: wgpu::Extent3d {
                    width: self.width,
                    height: self.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            }));
        }
    }

    /// Set the GLSL source for a shader (type: VERTEX_SHADER or FRAGMENT_SHADER).
    /// Translates GLSL to WGSL via naga.
    pub fn shader_source(&mut self, shader_type: u32, glsl: &str) {
        let wgsl = glsl_to_wgsl(glsl, shader_type == 35633 /* VERTEX_SHADER */);
        match wgsl {
            Ok(w) => {
                if shader_type == 35633 {
                    self.vertex_wgsl = Some(w);
                } else {
                    self.fragment_wgsl = Some(w);
                }
            }
            Err(e) => {
                tracing::warn!("[webgl] GLSL→WGSL translation failed: {}", e);
                self.error = 35702; // COMPILE_STATUS = false → error
            }
        }
    }

    /// Link program: check both shaders are present.
    pub fn link_program(&mut self) -> bool {
        self.program_active = self.vertex_wgsl.is_some() && self.fragment_wgsl.is_some();
        self.program_active
    }

    pub fn use_program(&mut self) {
        // Program selection is implicit — we use whatever was last linked.
    }

    pub fn clear_color(&mut self, r: f64, g: f64, b: f64, a: f64) {
        self.clear_color = [r, g, b, a];
    }

    pub fn viewport(&mut self, _x: i32, _y: i32, w: i32, h: i32) {
        if w > 0 && h > 0 && (w as u32 != self.width || h as u32 != self.height) {
            self.width = w as u32;
            self.height = h as u32;
            self.target = None; // recreate on next render
            self.ensure_target();
        }
    }

    /// Clear the render target.
    pub fn clear(&mut self, mask: u32) {
        if mask & 16384 == 0 {
            return; // COLOR_BUFFER_BIT not set
        }
        let Some(ref target) = self.target else {
            return;
        };
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("aris webgl clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: self.clear_color[0],
                            g: self.clear_color[1],
                            b: self.clear_color[2],
                            a: self.clear_color[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
        }
        self.queue.submit(Some(encoder.finish()));
    }

    /// Draw arrays: render count vertices starting at offset.
    pub fn draw_arrays(&mut self, _mode: u32, offset: i32, count: i32) {
        if !self.program_active {
            self.error = 1282; // INVALID_OPERATION
            return;
        }
        let Some(ref vs_source) = self.vertex_wgsl else {
            return;
        };
        let Some(ref fs_source) = self.fragment_wgsl else {
            return;
        };
        let Some(ref target) = self.target else {
            return;
        };
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("aris webgl shader"),
                source: wgpu::ShaderSource::Wgsl(format!("{}\n{}", vs_source, fs_source).into()),
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("aris webgl pipeline"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Rgba8UnormSrgb,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("aris webgl draw"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: self.clear_color[0],
                            g: self.clear_color[1],
                            b: self.clear_color[2],
                            a: self.clear_color[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&pipeline);
            pass.draw(offset as u32..(offset + count) as u32, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
    }

    /// Read the rendered pixels as RGBA8. Returns None if no target.
    pub fn read_pixels(&self) -> Option<Vec<u8>> {
        let target = self.target.as_ref()?;
        let bytes_per_row_padded = ((self.width * 4 + 255) & !255) as usize;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("aris webgl readback"),
            size: (bytes_per_row_padded * self.height as usize) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row_padded as u32),
                    rows_per_image: Some(self.height),
                },
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        let data = slice.get_mapped_range().ok()?;
        // De-pad rows.
        let row_bytes = self.width as usize * 4;
        let mut pixels = Vec::with_capacity(row_bytes * self.height as usize);
        for row in 0..self.height as usize {
            let start = row * bytes_per_row_padded;
            pixels.extend_from_slice(&data[start..start + row_bytes]);
        }
        drop(data);
        staging.unmap();
        Some(pixels)
    }

    pub fn get_error(&self) -> u32 {
        self.error
    }
}

/// Translate GLSL source to WGSL using naga.
fn glsl_to_wgsl(glsl: &str, is_vertex: bool) -> Result<String, String> {
    use naga::front::glsl::Options;
    use naga::valid::{Capabilities, Validator};

    let mut parser = naga::front::glsl::Frontend::default();
    let module = parser
        .parse(
            &Options {
                stage: if is_vertex {
                    naga::ShaderStage::Vertex
                } else {
                    naga::ShaderStage::Fragment
                },
                defines: Default::default(),
            },
            glsl,
        )
        .map_err(|e| format!("GLSL parse: {:?}", e))?;

    let info = Validator::new(Default::default(), Capabilities::all())
        .validate(&module)
        .map_err(|e| format!("validation: {:?}", e))?;

    let output =
        naga::back::wgsl::write_string(&module, &info, naga::back::wgsl::WriterFlags::empty())
            .map_err(|e| format!("WGSL write: {:?}", e))?;

    // naga outputs the entry point with the original name; we need it to be
    // vs_main / fs_main for our pipeline. Replace the entry point name.
    let entry = if is_vertex { "vs_main" } else { "fs_main" };
    let wgsl = output.replace("fn main", &format!("fn {}", entry));
    Ok(wgsl)
}
