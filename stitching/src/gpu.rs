//! Stitching on the GPU with wgpu compute shaders (in `shaders/`).
//!
//! When the [`StitchConfig`] changes, a lookup table is computed for each
//! camera saying where it saw each output pixel, along with the seam's
//! weights blurred for each band of the blend. Then for each frame, both
//! cameras' images are warped through their tables, blurred into Gaussian
//! levels, blended band by band and collapsed (see
//! [`Blend`](crate::geometry::Blend)). A level is stored at half the size of
//! the one before once it is blurred enough not to lose anything by it. The
//! levels hold premultiplied colors normalized at every level, so the area
//! past each camera's image is filled in from the image around it instead
//! of blending in black.
//!
//! Colors stay in the YCbCr of the cameras' JPEGs until the result is packed
//! on the GPU into a planar YUV 4:2:0 frame, ready to be compressed or
//! encoded.

use std::collections::VecDeque;
use std::sync::{OnceLock, mpsc};

use anyhow::{Context, Result, bail, ensure};
use bytemuck::{Pod, Zeroable};
use protocol::Side;

use crate::geometry::StitchConfig;
use crate::overlay::RgbaImage;

/// Width in input pixels over which a camera's image fades out at its edge.
const EDGE_FADE: f32 = 2.0;
/// The blur, in a blend level's own pixels, that lets it be half the size of
/// the level before.
const LEVEL_SIGMA: f64 = 1.0;
const ACROSS: [i32; 2] = [1, 0];
const DOWN: [i32; 2] = [0, 1];
const WORKGROUP_SIZE: u32 = 8;
/// Frames on the GPU before [`Stitcher::submit`] waits for the oldest.
const FRAMES_IN_FLIGHT: usize = 2;

/// A GPU device to stitch on.
#[derive(Clone)]
pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pub name: String,
}

impl Gpu {
    /// The process's GPU, opened the first time it's asked for.
    pub fn shared() -> Result<Gpu> {
        static SHARED: OnceLock<Gpu> = OnceLock::new();
        if let Some(gpu) = SHARED.get() {
            return Ok(gpu.clone());
        }
        let gpu = Gpu::open()?;
        Ok(SHARED.get_or_init(|| gpu).clone())
    }

    fn open() -> Result<Gpu> {
        pollster::block_on(async {
            let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
                backends: wgpu::Backends::PRIMARY,
                ..Default::default()
            });
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    force_fallback_adapter: false,
                    compatible_surface: None,
                })
                .await
                .context("finding a GPU")?;
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor {
                    label: Some("stitching"),
                    required_limits: adapter.limits(),
                    ..Default::default()
                })
                .await
                .context("opening the GPU")?;
            let name = adapter.get_info().name;
            tracing::info!(gpu = name, "opened the GPU");
            Ok(Gpu {
                device,
                queue,
                name,
            })
        })
    }
}

/// One camera's frame: planar YUV 4:2:0 with full range samples, as
/// decoded from its JPEGs.
#[derive(Clone, Copy)]
pub struct YuvPlanes<'a> {
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
}

/// What the output is for, which decides its colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Output {
    /// JPEG's YCbCr: BT.601 with samples from 0 to 255.
    Jpeg,
    /// HD video's YCbCr: BT.709 with samples from 16 to 235 (240 for
    /// chroma).
    Video,
}

/// What to draw over the stitched frames.
pub struct OverlayImage<'a> {
    pub image: &'a RgbaImage,
    /// Of the image's top left corner in the output.
    pub position: (i64, i64),
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LutUniform {
    rotation: [[f32; 4]; 3],
    focal: [f32; 2],
    center: [f32; 2],
    distortion: [f32; 4],
    input_size: [f32; 2],
    max_angle: f32,
    edge: f32,
    output_size: [u32; 2],
    output_focal: f32,
    middle_height: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BlurUniform {
    direction: [i32; 2],
    radius: i32,
    sigma: f32,
    step: i32,
    padding: i32,
}

impl BlurUniform {
    /// Blurring by `sigma` source pixels along `direction`, sampling every
    /// `step` of the `extent` source pixels along it.
    fn new(direction: [i32; 2], sigma: f64, step: u32, extent: u32) -> BlurUniform {
        let sigma = (sigma as f32).max(f32::EPSILON);
        BlurUniform {
            direction,
            radius: ((3.0 * sigma).ceil() as u32).min(extent) as i32,
            sigma,
            step: step as i32,
            padding: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct PackUniform {
    size: [u32; 2],
    video: u32,
    overlay_shown: u32,
    overlay_origin: [i32; 2],
    padding: [u32; 2],
}

struct Pipelines {
    lut: wgpu::ComputePipeline,
    warp: wgpu::ComputePipeline,
    seam: wgpu::ComputePipeline,
    blur: wgpu::ComputePipeline,
    restrict: wgpu::ComputePipeline,
    blend_top: wgpu::ComputePipeline,
    blend: wgpu::ComputePipeline,
    pack_luma: wgpu::ComputePipeline,
    pack_chroma: wgpu::ComputePipeline,
}

impl Pipelines {
    fn new(device: &wgpu::Device) -> Pipelines {
        const PYRAMID: &str = include_str!("shaders/pyramid.wgsl");
        let pipeline = |name: &str, source: String, entry_point: &str| {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(name),
                source: wgpu::ShaderSource::Wgsl(source.into()),
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(name),
                layout: None,
                module: &module,
                entry_point: Some(entry_point),
                compilation_options: Default::default(),
                cache: None,
            })
        };
        let with_pyramid = |source: &str| format!("{PYRAMID}\n{source}");
        let pack = include_str!("shaders/pack.wgsl");
        Pipelines {
            lut: pipeline("lut", include_str!("shaders/lut.wgsl").into(), "main"),
            warp: pipeline("warp", include_str!("shaders/warp.wgsl").into(), "main"),
            seam: pipeline("seam", include_str!("shaders/seam.wgsl").into(), "main"),
            blur: pipeline("blur", include_str!("shaders/blur.wgsl").into(), "main"),
            restrict: pipeline(
                "restrict",
                include_str!("shaders/restrict.wgsl").into(),
                "main",
            ),
            blend_top: pipeline(
                "blend_top",
                with_pyramid(include_str!("shaders/blend_top.wgsl")),
                "main",
            ),
            blend: pipeline(
                "blend",
                with_pyramid(include_str!("shaders/blend.wgsl")),
                "main",
            ),
            pack_luma: pipeline("pack_luma", pack.into(), "luma"),
            pack_chroma: pipeline("pack_chroma", pack.into(), "chroma"),
        }
    }
}

/// A texture and the view of it the shaders use.
struct Image {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

impl Image {
    fn new(
        device: &wgpu::Device,
        (width, height): (u32, u32),
        format: wgpu::TextureFormat,
        usage: wgpu::TextureUsages,
    ) -> Image {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        Image { texture, view }
    }

    /// Written by one pass and read by the next.
    fn intermediate(device: &wgpu::Device, size: (u32, u32)) -> Image {
        Image::new(
            device,
            size,
            wgpu::TextureFormat::Rgba16Float,
            wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        )
    }

    /// Uploaded from the CPU.
    fn uploaded(device: &wgpu::Device, size: (u32, u32), format: wgpu::TextureFormat) -> Image {
        Image::new(
            device,
            size,
            format,
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        )
    }

    fn size(&self) -> (u32, u32) {
        let size = self.texture.size();
        (size.width, size.height)
    }

    fn write(&self, queue: &wgpu::Queue, data: &[u8], bytes_per_pixel: u32) {
        let size = self.texture.size();
        queue.write_texture(
            self.texture.as_image_copy(),
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size.width * bytes_per_pixel),
                rows_per_image: Some(size.height),
            },
            size,
        );
    }
}

/// A camera's frame on the GPU.
struct InputPlanes {
    y: Image,
    u: Image,
    v: Image,
}

impl InputPlanes {
    fn new(device: &wgpu::Device, (width, height): (u32, u32)) -> InputPlanes {
        let format = wgpu::TextureFormat::R8Unorm;
        InputPlanes {
            y: Image::uploaded(device, (width, height), format),
            u: Image::uploaded(device, (width / 2, height / 2), format),
            v: Image::uploaded(device, (width / 2, height / 2), format),
        }
    }
}

fn bind_group(
    device: &wgpu::Device,
    pipeline: &wgpu::ComputePipeline,
    resources: &[wgpu::BindingResource],
) -> wgpu::BindGroup {
    let entries: Vec<wgpu::BindGroupEntry> = resources
        .iter()
        .enumerate()
        .map(|(binding, resource)| wgpu::BindGroupEntry {
            binding: binding as u32,
            resource: resource.clone(),
        })
        .collect();
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &pipeline.get_bind_group_layout(0),
        entries: &entries,
    })
}

/// Runs `pipeline` over a `width` by `height` grid.
fn dispatch(
    pass: &mut wgpu::ComputePass,
    pipeline: &wgpu::ComputePipeline,
    bind_group: &wgpu::BindGroup,
    (width, height): (u32, u32),
) {
    pass.set_pipeline(pipeline);
    pass.set_bind_group(0, bind_group, &[]);
    pass.dispatch_workgroups(
        width.div_ceil(WORKGROUP_SIZE),
        height.div_ceil(WORKGROUP_SIZE),
        1,
    );
}

fn view(image: &Image) -> wgpu::BindingResource<'_> {
    wgpu::BindingResource::TextureView(&image.view)
}

/// The pack passes' bind groups, which change with the overlay's texture.
struct PackBindGroups {
    luma: wgpu::BindGroup,
    chroma: wgpu::BindGroup,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Level {
    size: (u32, u32),
    /// Output pixels across each of the level's pixels.
    scale: u32,
}

/// The blend's levels for an output `size` big whose bands are blurred by
/// `sigmas` output pixels, finest first. Each level is half the size of the
/// one before if it's blurred by at least [`LEVEL_SIGMA`] of the smaller
/// pixels.
fn level_layout(size: (u32, u32), sigmas: &[f64]) -> Vec<Level> {
    let mut level = Level { size, scale: 1 };
    let mut layout = vec![level];
    for &sigma in sigmas.iter().skip(1) {
        let (width, height) = level.size;
        if width.min(height) >= 2 && sigma / (2 * level.scale) as f64 >= LEVEL_SIGMA {
            level = Level {
                size: (width.div_ceil(2), height.div_ceil(2)),
                scale: level.scale * 2,
            };
        }
        layout.push(level);
    }
    layout
}

/// The blend's finest level, which is the output's size whatever the blend.
struct FinestLevel {
    /// Each camera's image warped.
    images: [Image; 2],
    weights: Image,
    result: Image,
}

/// The blend's levels for a [`level_layout`], and the passes between them.
struct Levels {
    layout: Vec<Level>,
    /// By level, blurring it into the next across and then down.
    image_blur_uniforms: Vec<[wgpu::Buffer; 2]>,
    weight_blur_uniforms: Vec<[wgpu::Buffer; 2]>,
    image_blur_bind_groups: [Vec<[wgpu::BindGroup; 2]>; 2],
    weight_blur_bind_groups: Vec<[wgpu::BindGroup; 2]>,
    blend_top_bind_group: wgpu::BindGroup,
    /// By level but the coarsest.
    blend_bind_groups: Vec<wgpu::BindGroup>,
}

impl Levels {
    fn new(
        device: &wgpu::Device,
        pipelines: &Pipelines,
        finest: &FinestLevel,
        layout: Vec<Level>,
    ) -> Levels {
        let steps = layout.len() - 1;
        let blur_uniforms = || -> Vec<[wgpu::Buffer; 2]> {
            (0..steps)
                .map(|_| {
                    [(); 2].map(|()| {
                        device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("blur"),
                            size: size_of::<BlurUniform>() as u64,
                            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        })
                    })
                })
                .collect()
        };
        let image_blur_uniforms = blur_uniforms();
        let weight_blur_uniforms = blur_uniforms();

        let coarser_levels = || -> Vec<Image> {
            layout[1..]
                .iter()
                .map(|level| Image::intermediate(device, level.size))
                .collect()
        };
        let coarser_images = [coarser_levels(), coarser_levels()];
        let coarser_weights = coarser_levels();
        let coarser_results = coarser_levels();
        fn all_levels<'a>(finest: &'a Image, coarser: &'a [Image]) -> Vec<&'a Image> {
            std::iter::once(finest).chain(coarser).collect()
        }
        let images = [0, 1].map(|side| all_levels(&finest.images[side], &coarser_images[side]));
        let weights = all_levels(&finest.weights, &coarser_weights);
        let results = all_levels(&finest.result, &coarser_results);

        let blur_bind_groups = |levels: &[&Image], uniforms: &[[wgpu::Buffer; 2]]| {
            (0..steps)
                .map(|level| {
                    let across = Image::intermediate(
                        device,
                        (layout[level + 1].size.0, layout[level].size.1),
                    );
                    [
                        bind_group(
                            device,
                            &pipelines.blur,
                            &[
                                uniforms[level][0].as_entire_binding(),
                                view(levels[level]),
                                view(&across),
                            ],
                        ),
                        bind_group(
                            device,
                            &pipelines.blur,
                            &[
                                uniforms[level][1].as_entire_binding(),
                                view(&across),
                                view(levels[level + 1]),
                            ],
                        ),
                    ]
                })
                .collect()
        };
        let image_blur_bind_groups =
            [0, 1].map(|side| blur_bind_groups(&images[side], &image_blur_uniforms));
        let weight_blur_bind_groups = blur_bind_groups(&weights, &weight_blur_uniforms);

        let blend_top_bind_group = bind_group(
            device,
            &pipelines.blend_top,
            &[
                view(images[0][steps]),
                view(images[1][steps]),
                view(weights[steps]),
                view(results[steps]),
            ],
        );
        let blend_bind_groups = (0..steps)
            .map(|level| {
                bind_group(
                    device,
                    &pipelines.blend,
                    &[
                        view(images[0][level]),
                        view(images[0][level + 1]),
                        view(images[1][level]),
                        view(images[1][level + 1]),
                        view(weights[level]),
                        view(results[level + 1]),
                        view(results[level]),
                    ],
                )
            })
            .collect();
        Levels {
            layout,
            image_blur_uniforms,
            weight_blur_uniforms,
            image_blur_bind_groups,
            weight_blur_bind_groups,
            blend_top_bind_group,
            blend_bind_groups,
        }
    }

    /// Blurs the finest level of the images or weights `bind_groups` are
    /// for into each coarser one in turn.
    fn dispatch_blurs(
        &self,
        pass: &mut wgpu::ComputePass,
        pipelines: &Pipelines,
        bind_groups: &[[wgpu::BindGroup; 2]],
    ) {
        for (pair, [across, down]) in self.layout.windows(2).zip(bind_groups) {
            let (fine, coarse) = (pair[0], pair[1]);
            dispatch(pass, &pipelines.blur, across, (coarse.size.0, fine.size.1));
            dispatch(pass, &pipelines.blur, down, coarse.size);
        }
    }
}

/// Stitches pairs of `input_size` frames into `output_size` ones.
pub struct Stitcher {
    gpu: Gpu,
    input_size: (u32, u32),
    output_size: (u32, u32),
    output: Output,
    pipelines: Pipelines,
    inputs: [InputPlanes; 2],
    lut_uniforms: [wgpu::Buffer; 2],
    blur_uniforms: [wgpu::Buffer; 2],
    pack_uniform: wgpu::Buffer,
    luts: [wgpu::Buffer; 2],
    finest: FinestLevel,
    levels: Levels,
    overlay: Image,
    overlay_shown: bool,
    packed: wgpu::Buffer,
    staging: Vec<wgpu::Buffer>,
    lut_bind_groups: [wgpu::BindGroup; 2],
    seam_bind_group: wgpu::BindGroup,
    blur_bind_groups: [wgpu::BindGroup; 2],
    restrict_bind_group: wgpu::BindGroup,
    warp_bind_groups: [wgpu::BindGroup; 2],
    pack_bind_groups: PackBindGroups,
    /// The staging buffers of frames still on the GPU, oldest first.
    in_flight: VecDeque<(usize, wgpu::SubmissionIndex)>,
    next_staging: usize,
    /// The frame last read back.
    frame: Vec<u8>,
}

impl Stitcher {
    pub fn new(
        gpu: &Gpu,
        input_size: (u32, u32),
        output_size: (u32, u32),
        output: Output,
        config: &StitchConfig,
    ) -> Result<Stitcher> {
        ensure!(
            input_size.0 % 2 == 0 && input_size.1 % 2 == 0,
            "the input's size must be even"
        );
        ensure!(
            output_size.0 % 8 == 0 && output_size.1 % 2 == 0,
            "the output's width must be a multiple of 8 and its height even"
        );
        let device = &gpu.device;
        let pipelines = Pipelines::new(device);
        let buffer = |label: &str, size: u64, usage: wgpu::BufferUsages| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        let uniform_usage = wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST;
        let lut_uniforms =
            [(); 2].map(|()| buffer("lut", size_of::<LutUniform>() as u64, uniform_usage));
        let blur_uniforms =
            [(); 2].map(|()| buffer("blur", size_of::<BlurUniform>() as u64, uniform_usage));
        let pack_uniform = buffer("pack", size_of::<PackUniform>() as u64, uniform_usage);
        let pixels = output_size.0 as u64 * output_size.1 as u64;
        let luts = [(); 2].map(|()| {
            buffer(
                "lut",
                pixels * size_of::<[f32; 4]>() as u64,
                wgpu::BufferUsages::STORAGE,
            )
        });
        let frame_bytes = pixels * 3 / 2;
        let packed = buffer(
            "packed",
            frame_bytes,
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        let staging = (0..FRAMES_IN_FLIGHT)
            .map(|_| {
                buffer(
                    "staging",
                    frame_bytes,
                    wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                )
            })
            .collect();

        let finest = FinestLevel {
            images: [(); 2].map(|()| Image::intermediate(device, output_size)),
            weights: Image::intermediate(device, output_size),
            result: Image::intermediate(device, output_size),
        };
        let levels = Levels::new(
            device,
            &pipelines,
            &finest,
            level_layout(output_size, &config.blend.sigmas_for(output_size.1)),
        );
        let scratch = [(); 2].map(|()| Image::intermediate(device, output_size));
        let inputs = [(); 2].map(|()| InputPlanes::new(device, input_size));
        let overlay = Image::uploaded(device, (1, 1), wgpu::TextureFormat::Rgba8Unorm);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let lut_bind_groups = [0, 1].map(|side| {
            bind_group(
                device,
                &pipelines.lut,
                &[
                    lut_uniforms[side].as_entire_binding(),
                    luts[side].as_entire_binding(),
                ],
            )
        });
        let seam_bind_group = bind_group(
            device,
            &pipelines.seam,
            &[
                luts[0].as_entire_binding(),
                luts[1].as_entire_binding(),
                view(&scratch[0]),
            ],
        );
        let blur_bind_groups = [(0, 1), (1, 0)].map(|(from, to)| {
            bind_group(
                device,
                &pipelines.blur,
                &[
                    blur_uniforms[from].as_entire_binding(),
                    view(&scratch[from]),
                    view(&scratch[to]),
                ],
            )
        });
        let restrict_bind_group = bind_group(
            device,
            &pipelines.restrict,
            &[
                luts[0].as_entire_binding(),
                luts[1].as_entire_binding(),
                view(&scratch[0]),
                view(&finest.weights),
            ],
        );
        let warp_bind_groups = [0, 1].map(|side| {
            bind_group(
                device,
                &pipelines.warp,
                &[
                    luts[side].as_entire_binding(),
                    view(&inputs[side].y),
                    view(&inputs[side].u),
                    view(&inputs[side].v),
                    wgpu::BindingResource::Sampler(&sampler),
                    view(&finest.images[side]),
                ],
            )
        });
        let pack_bind_groups = PackBindGroups::new(
            device,
            &pipelines,
            &pack_uniform,
            &finest.result,
            &luts,
            &overlay,
            &packed,
        );

        let mut stitcher = Stitcher {
            gpu: gpu.clone(),
            input_size,
            output_size,
            output,
            pipelines,
            inputs,
            lut_uniforms,
            blur_uniforms,
            pack_uniform,
            luts,
            finest,
            levels,
            overlay,
            overlay_shown: false,
            packed,
            staging,
            lut_bind_groups,
            seam_bind_group,
            blur_bind_groups,
            restrict_bind_group,
            warp_bind_groups,
            pack_bind_groups,
            in_flight: VecDeque::new(),
            next_staging: 0,
            frame: Vec::new(),
        };
        stitcher.write_pack_uniform((0, 0));
        stitcher.configure(config)?;
        Ok(stitcher)
    }

    pub fn input_size(&self) -> (u32, u32) {
        self.input_size
    }

    pub fn output_size(&self) -> (u32, u32) {
        self.output_size
    }

    /// Stitches the frames submitted from now on with `config`.
    pub fn configure(&mut self, config: &StitchConfig) -> Result<()> {
        config.calibration.validate()?;
        let view = &config.view;
        let cylinder_rotation = view.cylinder_rotation();
        for side in Side::BOTH {
            let camera = config.calibration.camera(side);
            let lens = camera.lens.scaled_to(self.input_size.0, self.input_size.1);
            let rotation = camera.orientation.rotation().inverse() * cylinder_rotation;
            let columns = [0, 1, 2].map(|index| {
                let column = rotation.matrix().column(index);
                [column.x as f32, column.y as f32, column.z as f32, 0.0]
            });
            let uniform = LutUniform {
                rotation: columns,
                focal: [lens.fx as f32, lens.fy as f32],
                center: [lens.cx as f32, lens.cy as f32],
                distortion: lens.distortion.map(|coefficient| coefficient as f32),
                input_size: [lens.width as f32, lens.height as f32],
                max_angle: lens.max_angle() as f32,
                edge: EDGE_FADE,
                output_size: [self.output_size.0, self.output_size.1],
                output_focal: view.focal_length(self.output_size) as f32,
                middle_height: view.middle_height() as f32,
            };
            self.gpu.queue.write_buffer(
                &self.lut_uniforms[side.index()],
                0,
                bytemuck::bytes_of(&uniform),
            );
        }
        let (width, height) = self.output_size;
        let sigmas = config.blend.sigmas_for(height);
        let layout = level_layout(self.output_size, &sigmas);
        if self.levels.layout != layout {
            self.levels = Levels::new(&self.gpu.device, &self.pipelines, &self.finest, layout);
        }
        let write_blur = |uniform: &wgpu::Buffer, blur: BlurUniform| {
            self.gpu
                .queue
                .write_buffer(uniform, 0, bytemuck::bytes_of(&blur));
        };
        write_blur(
            &self.blur_uniforms[0],
            BlurUniform::new(ACROSS, sigmas[0], 1, width),
        );
        write_blur(
            &self.blur_uniforms[1],
            BlurUniform::new(DOWN, sigmas[0], 1, height),
        );
        for (level, pair) in self.levels.layout.windows(2).enumerate() {
            let (fine, coarse) = (pair[0], pair[1]);
            let step = coarse.scale / fine.scale;
            let blurring_from = |sigma: f64| {
                (sigmas[level + 1].powi(2) - sigma.powi(2)).max(0.0).sqrt() / fine.scale as f64
            };
            let image_sigma = if level == 0 { 0.0 } else { sigmas[level] };
            for (uniforms, sigma) in [
                (&self.levels.image_blur_uniforms[level], blurring_from(image_sigma)),
                (&self.levels.weight_blur_uniforms[level], blurring_from(sigmas[level])),
            ] {
                write_blur(&uniforms[0], BlurUniform::new(ACROSS, sigma, step, fine.size.0));
                write_blur(&uniforms[1], BlurUniform::new(DOWN, sigma, step, fine.size.1));
            }
        }

        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            let pipelines = &self.pipelines;
            for bind_group in &self.lut_bind_groups {
                dispatch(&mut pass, &pipelines.lut, bind_group, self.output_size);
            }
            dispatch(
                &mut pass,
                &pipelines.seam,
                &self.seam_bind_group,
                self.output_size,
            );
            for bind_group in &self.blur_bind_groups {
                dispatch(&mut pass, &pipelines.blur, bind_group, self.output_size);
            }
            dispatch(
                &mut pass,
                &pipelines.restrict,
                &self.restrict_bind_group,
                self.output_size,
            );
            self.levels
                .dispatch_blurs(&mut pass, pipelines, &self.levels.weight_blur_bind_groups);
        }
        self.gpu.queue.submit([encoder.finish()]);
        Ok(())
    }

    /// Draws `overlay` over the frames submitted from now on, or stops
    /// drawing it.
    pub fn set_overlay(&mut self, overlay: Option<OverlayImage>) {
        let position = match overlay {
            None => {
                self.overlay_shown = false;
                (0, 0)
            }
            Some(OverlayImage { image, position }) => {
                let size = (image.width, image.height);
                if self.overlay.size() != size {
                    self.overlay =
                        Image::uploaded(&self.gpu.device, size, wgpu::TextureFormat::Rgba8Unorm);
                    self.pack_bind_groups = PackBindGroups::new(
                        &self.gpu.device,
                        &self.pipelines,
                        &self.pack_uniform,
                        &self.finest.result,
                        &self.luts,
                        &self.overlay,
                        &self.packed,
                    );
                }
                self.overlay.write(&self.gpu.queue, &image.pixels, 4);
                self.overlay_shown = true;
                position
            }
        };
        self.write_pack_uniform(position);
    }

    fn write_pack_uniform(&self, overlay_position: (i64, i64)) {
        let uniform = PackUniform {
            size: [self.output_size.0, self.output_size.1],
            video: (self.output == Output::Video) as u32,
            overlay_shown: self.overlay_shown as u32,
            overlay_origin: [overlay_position.0 as i32, overlay_position.1 as i32],
            padding: [0, 0],
        };
        self.gpu
            .queue
            .write_buffer(&self.pack_uniform, 0, bytemuck::bytes_of(&uniform));
    }

    /// Starts stitching a pair of frames (indexed by [`Side::index`]). Once
    /// enough frames are on the GPU, waits for the oldest and returns it:
    /// planar YUV 4:2:0 with colors for the stitcher's [`Output`].
    pub fn submit(&mut self, frames: [YuvPlanes; 2]) -> Result<Option<&[u8]>> {
        self.enqueue(frames)?;
        if self.in_flight.len() < FRAMES_IN_FLIGHT {
            return Ok(None);
        }
        self.read_oldest().map(Some)
    }

    /// Stitches a pair of frames and waits for the result, for when latency
    /// matters more than throughput. Frames [submitted](Stitcher::submit)
    /// earlier must be [finished](Stitcher::finish) first.
    pub fn stitch(&mut self, frames: [YuvPlanes; 2]) -> Result<&[u8]> {
        ensure!(self.in_flight.is_empty(), "earlier frames are still on the GPU");
        self.enqueue(frames)?;
        self.read_oldest()
    }

    fn enqueue(&mut self, frames: [YuvPlanes; 2]) -> Result<()> {
        let (width, height) = self.input_size;
        let luma = (width * height) as usize;
        for (input, frame) in self.inputs.iter().zip(frames) {
            if frame.y.len() != luma || frame.u.len() != luma / 4 || frame.v.len() != luma / 4 {
                bail!("a frame isn't {width}x{height} YUV 4:2:0");
            }
            input.y.write(&self.gpu.queue, frame.y, 1);
            input.u.write(&self.gpu.queue, frame.u, 1);
            input.v.write(&self.gpu.queue, frame.v, 1);
        }

        let mut encoder = self.gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            let pipelines = &self.pipelines;
            for bind_group in &self.warp_bind_groups {
                dispatch(&mut pass, &pipelines.warp, bind_group, self.output_size);
            }
            let levels = &self.levels;
            for bind_groups in &levels.image_blur_bind_groups {
                levels.dispatch_blurs(&mut pass, pipelines, bind_groups);
            }
            let top = levels.layout.len() - 1;
            dispatch(
                &mut pass,
                &pipelines.blend_top,
                &levels.blend_top_bind_group,
                levels.layout[top].size,
            );
            for level in (0..top).rev() {
                dispatch(
                    &mut pass,
                    &pipelines.blend,
                    &levels.blend_bind_groups[level],
                    levels.layout[level].size,
                );
            }
            let (width, height) = self.output_size;
            dispatch(
                &mut pass,
                &pipelines.pack_luma,
                &self.pack_bind_groups.luma,
                (width / 4, height),
            );
            dispatch(
                &mut pass,
                &pipelines.pack_chroma,
                &self.pack_bind_groups.chroma,
                (width / 8, height / 2),
            );
        }
        let staging = self.next_staging;
        self.next_staging = (staging + 1) % self.staging.len();
        encoder.copy_buffer_to_buffer(&self.packed, 0, &self.staging[staging], 0, None);
        let submission = self.gpu.queue.submit([encoder.finish()]);
        self.in_flight.push_back((staging, submission));
        Ok(())
    }

    /// Waits for the oldest frame still on the GPU and returns it; `None`
    /// once every frame submitted has been returned.
    pub fn finish(&mut self) -> Result<Option<&[u8]>> {
        if self.in_flight.is_empty() {
            return Ok(None);
        }
        self.read_oldest().map(Some)
    }

    fn read_oldest(&mut self) -> Result<&[u8]> {
        let (staging, submission) = self.in_flight.pop_front().context("no frame on the GPU")?;
        let buffer = &self.staging[staging];
        let slice = buffer.slice(..);
        let (sender, receiver) = mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.gpu
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .context("waiting for the GPU")?;
        receiver
            .recv()
            .context("the GPU dropped a frame")?
            .context("reading a frame back from the GPU")?;
        self.frame.clear();
        self.frame.extend_from_slice(&slice.get_mapped_range());
        buffer.unmap();
        Ok(&self.frame)
    }
}

impl PackBindGroups {
    fn new(
        device: &wgpu::Device,
        pipelines: &Pipelines,
        uniform: &wgpu::Buffer,
        result: &Image,
        luts: &[wgpu::Buffer; 2],
        overlay: &Image,
        packed: &wgpu::Buffer,
    ) -> PackBindGroups {
        let resources = [
            uniform.as_entire_binding(),
            wgpu::BindingResource::TextureView(&result.view),
            luts[0].as_entire_binding(),
            luts[1].as_entire_binding(),
            wgpu::BindingResource::TextureView(&overlay.view),
            packed.as_entire_binding(),
        ];
        PackBindGroups {
            luma: bind_group(device, &pipelines.pack_luma, &resources),
            chroma: bind_group(device, &pipelines.pack_chroma, &resources),
        }
    }
}

#[cfg(test)]
mod tests {
    use nalgebra::{Vector2, Vector3};

    use super::*;
    use crate::geometry::{Blend, Calibration, Orientation, View};

    const INPUT_SIZE: (u32, u32) = (384, 216);
    const OUTPUT_SIZE: (u32, u32) = (480, 240);

    /// A smooth pattern over the rig's surroundings, by direction.
    fn scene(direction: &Vector3<f64>) -> f64 {
        let azimuth = direction.x.atan2(direction.z);
        let elevation = (-direction.y).atan2(direction.xz().norm());
        0.5 + 0.25 * (4.0 * azimuth).sin() * (3.0 * elevation).cos()
    }

    /// What `camera` sees of [`scene`], in full range YUV 4:2:0.
    fn photograph(config: &StitchConfig, side: Side) -> Vec<u8> {
        let camera = config.calibration.camera(side);
        let lens = camera.lens.scaled_to(INPUT_SIZE.0, INPUT_SIZE.1);
        let to_rig = camera.orientation.rotation();
        let (width, height) = INPUT_SIZE;
        let mut frame = Vec::with_capacity((width * height * 3 / 2) as usize);
        for y in 0..height {
            for x in 0..width {
                let ray = to_rig * lens.unproject(&Vector2::new(x as f64, y as f64));
                frame.push((scene(&ray) * 255.0).round() as u8);
            }
        }
        frame.resize((width * height * 3 / 2) as usize, 128);
        frame
    }

    fn planes(frame: &[u8]) -> YuvPlanes<'_> {
        let luma = (INPUT_SIZE.0 * INPUT_SIZE.1) as usize;
        YuvPlanes {
            y: &frame[..luma],
            u: &frame[luma..luma * 5 / 4],
            v: &frame[luma * 5 / 4..],
        }
    }

    fn stitch(config: &StitchConfig, frames: [&[u8]; 2]) -> Option<Vec<u8>> {
        let gpu = match Gpu::shared() {
            Ok(gpu) => gpu,
            Err(error) => {
                eprintln!("skipping: {error:#}");
                return None;
            }
        };
        let mut stitcher = Stitcher::new(&gpu, INPUT_SIZE, OUTPUT_SIZE, Output::Jpeg, config)
            .expect("a stitcher");
        assert!(
            stitcher
                .submit(frames.map(planes))
                .expect("submitting")
                .is_none()
        );
        let frame = stitcher.finish().expect("stitching").expect("a frame").to_vec();
        assert!(stitcher.finish().expect("finishing").is_none());
        Some(frame)
    }

    fn rig() -> StitchConfig {
        StitchConfig::fitting(Calibration::nominal([
            Orientation {
                yaw: -35.0,
                pitch: -10.0,
                roll: 3.0,
            },
            Orientation {
                yaw: 35.0,
                pitch: -10.0,
                roll: -3.0,
            },
        ]))
    }

    /// Whether `pixel` of the output is seen well inside either camera's
    /// image.
    fn well_covered(config: &StitchConfig, pixel: &Vector2<f64>) -> bool {
        const MARGIN: f64 = 8.0;
        Side::BOTH.into_iter().any(|side| {
            config
                .source_of(side, pixel, OUTPUT_SIZE, INPUT_SIZE)
                .is_some_and(|(source, _)| {
                    source.x > MARGIN
                        && source.y > MARGIN
                        && source.x < INPUT_SIZE.0 as f64 - MARGIN
                        && source.y < INPUT_SIZE.1 as f64 - MARGIN
                })
        })
    }

    #[test]
    fn levels_halve_once_blurred_enough() {
        let level = |width, height, scale| Level {
            size: (width, height),
            scale,
        };
        assert_eq!(
            level_layout((480, 241), &[0.5, 1.5, 3.0, 3.5, 9.0]),
            [
                level(480, 241, 1),
                level(480, 241, 1),
                level(240, 121, 2),
                level(240, 121, 2),
                level(120, 61, 4),
            ]
        );
        assert_eq!(
            level_layout((2, 1), &[1.0, 4.0]),
            [level(2, 1, 1), level(2, 1, 1)]
        );
    }

    #[test]
    fn stitching_shows_the_scene_both_cameras_saw() {
        let blends = [
            Blend::default(),
            Blend {
                finest_sigma: 0.0,
                sigma_ratio: 1.5,
                layers: Blend::MAX_LAYERS,
            },
            Blend {
                finest_sigma: 6.0,
                sigma_ratio: 1.3,
                layers: 8,
            },
        ];
        for blend in blends {
            let config = StitchConfig { blend, ..rig() };
            let left = photograph(&config, Side::Left);
            let right = photograph(&config, Side::Right);
            let Some(frame) = stitch(&config, [&left, &right]) else {
                return;
            };
            let (width, height) = OUTPUT_SIZE;
            let to_rig = config.view.cylinder_rotation();
            let mut checked = 0;
            let mut worst: f64 = 0.0;
            for y in 0..height {
                for x in 0..width {
                    let pixel = Vector2::new(x as f64, y as f64);
                    if !well_covered(&config, &pixel) {
                        continue;
                    }
                    let direction = to_rig * config.view.direction_at(&pixel, OUTPUT_SIZE);
                    let expected = scene(&direction) * 255.0;
                    let actual = frame[(y * width + x) as usize] as f64;
                    worst = worst.max((actual - expected).abs());
                    checked += 1;
                }
            }
            assert!(checked > (width * height / 2) as usize, "only {checked} pixels covered");
            assert!(worst <= 3.0, "a pixel is {worst} off the scene with {blend:?}");
            let chroma = &frame[(width * height) as usize..];
            assert!(chroma.iter().all(|&sample| sample.abs_diff(128) <= 1));
        }
    }

    #[test]
    fn stitching_is_black_where_neither_camera_sees() {
        let config = StitchConfig {
            view: View {
                horizontal_fov: 300.0,
                ..rig().view
            },
            ..rig()
        };
        let white = vec![255; (INPUT_SIZE.0 * INPUT_SIZE.1 * 3 / 2) as usize];
        let Some(frame) = stitch(&config, [&white, &white]) else {
            return;
        };
        let width = OUTPUT_SIZE.0 as usize;
        let row = OUTPUT_SIZE.1 as usize / 2;
        assert_eq!(frame[row * width], 0, "behind the rig");
        assert_eq!(frame[row * width + width / 2], 255, "in front of the rig");
    }

    #[test]
    fn seam_blends_without_overshooting() {
        let config = rig();
        let size = (INPUT_SIZE.0 * INPUT_SIZE.1) as usize;
        let mut dark = vec![50; size];
        dark.resize(size * 3 / 2, 128);
        let mut bright = vec![200; size];
        bright.resize(size * 3 / 2, 128);
        let Some(frame) = stitch(&config, [&dark, &bright]) else {
            return;
        };
        let (width, height) = OUTPUT_SIZE;
        let row = height / 2;
        let samples: Vec<u8> = (0..width)
            .filter(|&x| well_covered(&config, &Vector2::new(x as f64, row as f64)))
            .map(|x| frame[(row * width + x) as usize])
            .collect();
        assert_eq!(samples.first(), Some(&50));
        assert_eq!(samples.last(), Some(&200));
        assert!(samples.windows(2).all(|pair| pair[0] <= pair[1]));
    }
}
