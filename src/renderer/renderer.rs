use anyhow::{Context, Ok, Result};

use crate::{
    entity::Scene,
    renderer::{
        particle::{ParticleRenderer, ParticleRendererBuilder},
        postprocessing::{BlurRenderPass, BrightPassRenderPass, ComposeRenderPass},
    },
    window::{Size, Window},
};

use super::postprocessing::{AddRenderPass, CopyRenderPass};

const HDR_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
const DEPTH_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

pub struct Renderer {
    surface: wgpu::Surface,
    surface_format: wgpu::TextureFormat,
    device: wgpu::Device,
    queue: wgpu::Queue,
    render_targets: RenderTargets,
    particle_renderer: ParticleRenderer,
    bright_pass_render_pass: BrightPassRenderPass,
    bloom_blur_render_pass: BlurRenderPass,
    bloom_combine_render_pass: AddRenderPass,
    bloom_blur_render_passes: Vec<BlurRenderPass>,
    bloom_combine_render_passes: Vec<CopyRenderPass>,
    compose_render_pass: ComposeRenderPass,
}

impl Renderer {
    pub async fn new(window: &impl Window, scene: &Scene) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::Backends::PRIMARY);
        let surface = unsafe { instance.create_surface(&window) };

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::default(),
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("No adapter found")?;

        let surface_format = surface
            .get_preferred_format(&adapter)
            .context("No preferred format found")?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await?;

        let Size { width, height } = window.size();

        Self::configure_surface(&surface, &device, surface_format, width, height);

        let render_targets = RenderTargets::new(&device, width, height);

        let particle_renderer = ParticleRendererBuilder::new(scene)
            .color_target_format(HDR_TEXTURE_FORMAT)
            .depth_format(DEPTH_TEXTURE_FORMAT)
            .build(&device);

        let bright_pass_render_pass =
            BrightPassRenderPass::new(&device, &render_targets.color, HDR_TEXTURE_FORMAT);

        let bloom_blur_render_pass =
            BlurRenderPass::new(&device, &render_targets.bright_pass, HDR_TEXTURE_FORMAT);

        let bloom_blur_render_passes = {
            let all_blur_texture_views_but_last = render_targets
                .bloom_blur
                .iter()
                .take(render_targets.bloom_blur.len() - 1);
            let src_texture_views =
                std::iter::once(&render_targets.bright_pass).chain(all_blur_texture_views_but_last);

            src_texture_views
                .map(|src_texture_view| {
                    BlurRenderPass::new(&device, src_texture_view, HDR_TEXTURE_FORMAT)
                })
                .collect::<Vec<_>>()
        };

        let bloom_combine_render_pass = AddRenderPass::new(
            &device,
            &[&render_targets.bloom_blur[0], &render_targets.bloom_blur[1]],
            HDR_TEXTURE_FORMAT,
        );

        let bloom_combine_render_passes = render_targets
            .bloom_blur
            .iter()
            .map(|texture_view| CopyRenderPass::new(&device, texture_view, HDR_TEXTURE_FORMAT))
            .collect::<Vec<_>>();

        let compose_render_pass = ComposeRenderPass::new(
            &device,
            &render_targets.color,
            &render_targets.bloom,
            surface_format,
        );

        Ok(Self {
            surface,
            surface_format,
            device,
            queue,
            render_targets,
            particle_renderer,
            bright_pass_render_pass,
            bloom_blur_render_pass,
            bloom_combine_render_pass,
            bloom_blur_render_passes,
            bloom_combine_render_passes,
            compose_render_pass,
        })
    }

    fn configure_surface(
        surface: &wgpu::Surface,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        surface.configure(
            device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format,
                width,
                height,
                present_mode: wgpu::PresentMode::Fifo,
            },
        )
    }

    pub fn resize(&mut self, size: Size) {
        let Size { width, height } = size;
        Self::configure_surface(
            &self.surface,
            &self.device,
            self.surface_format,
            width,
            height,
        );
        self.render_targets = RenderTargets::new(&self.device, width, height);
        self.bright_pass_render_pass =
            BrightPassRenderPass::new(&self.device, &self.render_targets.color, HDR_TEXTURE_FORMAT);
        self.bloom_blur_render_pass = BlurRenderPass::new(
            &self.device,
            &self.render_targets.bright_pass,
            HDR_TEXTURE_FORMAT,
        );
        self.compose_render_pass = ComposeRenderPass::new(
            &self.device,
            &self.render_targets.color,
            &self.render_targets.bloom,
            self.surface_format,
        );
    }

    pub fn render(&mut self, scene: &Scene) {
        self.particle_renderer.update(&self.queue, scene);
        self.bright_pass_render_pass.update(&self.queue, scene);
        self.compose_render_pass.update(&self.queue, scene);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Particle Render Pass"),
                color_attachments: &[wgpu::RenderPassColorAttachment {
                    view: &self.render_targets.color,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: true,
                    },
                }],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.render_targets.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: false,
                    }),
                    stencil_ops: None,
                }),
            });
            self.particle_renderer.draw(&mut rpass);
        }

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Bright Pass Render Pass"),
                color_attachments: &[wgpu::RenderPassColorAttachment {
                    view: &self.render_targets.bright_pass,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: true,
                    },
                }],
                depth_stencil_attachment: None,
            });
            self.bright_pass_render_pass.draw(&mut rpass);
        }

        // {
        //     let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        //         label: Some("Bloom Blur Render Pass"),
        //         color_attachments: &[wgpu::RenderPassColorAttachment {
        //             view: &self.render_targets.bloom_blur[0],
        //             resolve_target: None,
        //             ops: wgpu::Operations {
        //                 load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
        //                 store: true,
        //             },
        //         }],
        //         depth_stencil_attachment: None,
        //     });
        //     self.bloom_blur_render_pass.draw(&mut rpass);
        // }

        for i in 0..self.render_targets.bloom_blur.len() {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some(format!("Bloom Blur Render Pass {}", i).as_str()),
                color_attachments: &[wgpu::RenderPassColorAttachment {
                    view: &self.render_targets.bloom_blur[i],
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: true,
                    },
                }],
                depth_stencil_attachment: None,
            });
            self.bloom_blur_render_passes[i].draw(&mut rpass);
        }

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Bloom Combine Render Pass"),
                color_attachments: &[wgpu::RenderPassColorAttachment {
                    view: &self.render_targets.bloom,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: true,
                    },
                }],
                depth_stencil_attachment: None,
            });
            // self.bloom_combine_render_pass.draw(&mut rpass);
            for render_pass in &self.bloom_combine_render_passes {
                render_pass.draw(&mut rpass);
            }
        }

        let surface_texture = self
            .surface
            .get_current_texture()
            .expect("Failed to get next surface texture");

        let surface_texture_view = surface_texture.texture.create_view(&Default::default());

        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("Compose Render Pass"),
                color_attachments: &[wgpu::RenderPassColorAttachment {
                    view: &surface_texture_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: true,
                    },
                }],
                depth_stencil_attachment: None,
            });
            self.compose_render_pass.draw(&mut rpass);
        }

        self.queue.submit(std::iter::once(encoder.finish()));

        surface_texture.present();
    }
}

struct RenderTargets {
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    bright_pass: wgpu::TextureView,
    bloom_blur: Vec<wgpu::TextureView>,
    bloom: wgpu::TextureView,
}

impl RenderTargets {
    fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let color = Self::create_render_target_texture_view(
            device,
            "Color Texture",
            width,
            height,
            HDR_TEXTURE_FORMAT,
        );
        let depth = Self::create_render_target_texture_view(
            device,
            "Depth Texture",
            width,
            height,
            DEPTH_TEXTURE_FORMAT,
        );
        let bright_pass = Self::create_render_target_texture_view(
            device,
            "Bright Pass Texture",
            width / 4,
            height / 4,
            HDR_TEXTURE_FORMAT,
        );
        let bloom_blur = (0..16)
            .map(|i| {
                Self::create_render_target_texture_view(
                    device,
                    format!("Blur Texture {}", i).as_str(),
                    width / 4,
                    height / 4,
                    HDR_TEXTURE_FORMAT,
                )
            })
            .collect::<Vec<_>>();
        let bloom = Self::create_render_target_texture_view(
            device,
            "Bloom Texture",
            width / 4,
            height / 4,
            HDR_TEXTURE_FORMAT,
        );

        Self {
            color,
            depth,
            bright_pass,
            bloom_blur,
            bloom,
        }
    }

    fn create_render_target_texture_view(
        device: &wgpu::Device,
        label: &str,
        width: u32,
        height: u32,
        format: wgpu::TextureFormat,
    ) -> wgpu::TextureView {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT,
        });
        texture.create_view(&wgpu::TextureViewDescriptor::default())
    }
}