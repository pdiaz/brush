use super::sync_span::SyncSpan as SyncSpanRaw;
use crate::camera::Camera;
use crate::dim_check::DimCheck;
use crate::kernels::{
    GetTileBinEdges, MapGaussiansToIntersect, ProjectBackwards, ProjectSplats, Rasterize,
    RasterizeBackwards,
};
use crate::shaders::get_tile_bin_edges::VERTICAL_GROUPS;
use brush_kernel::{bitcast_tensor, create_tensor, BurnBack, SplatKernel};
use brush_prefix_sum::prefix_sum;
use brush_sort::radix_argsort;
use burn::backend::autodiff::NodeID;
use burn::tensor::ops::IntTensorOps;
use burn::tensor::ops::{FloatTensor, FloatTensorOps};
use burn::tensor::Tensor;

use tracing::info_span;

use super::{shaders, Backend, RenderAux};
use burn::backend::{
    autodiff::{
        checkpoint::{base::Checkpointer, strategy::CheckpointStrategy},
        grads::Gradients,
        ops::{Backward, Ops, OpsKind},
    },
    Autodiff,
};
use glam::{uvec2, Vec3, Vec3Swizzles};

// Use an alias so we don't have to type out the backend every time.
type SyncSpan<'a> = SyncSpanRaw<'a, BurnBack>;

pub fn num_sh_coeffs(degree: usize) -> usize {
    (degree + 1).pow(2)
}

pub fn sh_degree_from_coeffs(coeffs_per_channel: usize) -> usize {
    match coeffs_per_channel {
        1 => 0,
        4 => 1,
        9 => 2,
        16 => 3,
        25 => 4,
        _ => panic!("Invalid nr. of sh bases {coeffs_per_channel}"),
    }
}

fn render_forward(
    camera: &Camera,
    img_size: glam::UVec2,
    means: Tensor<BurnBack, 2>,
    _xy_dummy: Tensor<BurnBack, 2>,
    log_scales: Tensor<BurnBack, 2>,
    quats: Tensor<BurnBack, 2>,
    sh_coeffs: Tensor<BurnBack, 2>,
    raw_opacities: Tensor<BurnBack, 1>,
    background: glam::Vec3,
    raster_u32: bool,
) -> (Tensor<BurnBack, 3>, RenderAux<BurnBack>) {
    let _render_span = info_span!("render_gaussians").entered();
    let device = &means.device().clone();

    let setup_span = SyncSpan::new("setup", device);

    // Check whether dimesions are valid.
    DimCheck::new()
        .check_dims(&means, ["D".into(), 3.into()])
        .check_dims(&log_scales, ["D".into(), 3.into()])
        .check_dims(&quats, ["D".into(), 4.into()])
        .check_dims(&sh_coeffs, ["D".into(), "C".into()])
        .check_dims(&raw_opacities, ["D".into()]);

    // Divide screen into tiles.
    let tile_bounds = uvec2(
        img_size.x.div_ceil(shaders::rasterize::TILE_WIDTH),
        img_size.y.div_ceil(shaders::rasterize::TILE_WIDTH),
    );

    let num_points = means.dims()[0];

    // Projected gaussian values.
    let client = &means.clone().into_primitive().client;

    let xys = create_tensor::<f32, 2>([num_points, 2], device, client);
    let depths = create_tensor::<f32, 1>([num_points], device, client);
    let colors = create_tensor::<f32, 2>([num_points, 4], device, client);
    let radii = create_tensor::<u32, 1>([num_points], device, client);
    let conic_comps = create_tensor::<f32, 2>([num_points, 4], device, client);

    // A note on some confusing naming that'll be used throughout this function:
    // Gaussians are stored in various states of buffers, eg. at the start they're all in one big bufffer,
    // then we sparsely store some results, then sort gaussian based on depths, etc.
    // Overall this means there's lots of indices flying all over the place, and it's hard to keep track
    // what is indexing what. So, for some sanity, try to match a few "gaussian ids" (gid) variable names.
    // - Global Gaussin ID - global_gid
    // - Compacted Gaussian ID - compact_gid
    // - Compacted Gaussian ID sorted by depth - depthsort_gid
    // - Per tile intersection depth sorted ID - tiled_gid
    // - Sorted by tile per tile intersection depth sorted ID - sorted_tiled_gid
    // Then, various buffers map between these, which are named x_from_y_gid, eg.
    //  global_from_compact_gid or compact_from_depthsort_gid.

    // Tile rendering setup.

    // Number of tiles hit per splat.
    let num_tiles_hit = BurnBack::int_zeros([num_points].into(), device);
    // Atomic counter of number of visible splats.
    let num_visible = bitcast_tensor(BurnBack::int_zeros([1].into(), device));
    // Compaction buffer permutation.
    let global_from_compact_gid = create_tensor::<u32, 1>([num_points], device, client);

    // TODO: This should just be:
    // let arranged_ids = BurnBack::int_arange();
    // but atm Burn only has a CPU version of arange which is way too slow :/
    // Instead just fill this in the kernel, not great but it works.
    let arranged_ids = create_tensor::<u32, 1>([num_points], device, client);

    drop(setup_span);

    let sh_degree = sh_degree_from_coeffs(sh_coeffs.dims()[1] / 3);

    {
        let _span = SyncSpan::new("ProjectSplats", device);

        ProjectSplats::new().execute(
            client,
            shaders::project_forward::Uniforms {
                viewmat: camera.world_to_local().to_cols_array_2d(),
                focal: camera.focal(img_size).into(),
                pixel_center: camera.center(img_size).into(),
                img_size: img_size.into(),
                clip_thresh: 0.01,
                sh_degree: sh_degree as u32,
            },
            &[
                means.into_primitive().handle.binding(),
                log_scales.into_primitive().handle.binding(),
                quats.into_primitive().handle.binding(),
                sh_coeffs.into_primitive().handle.binding(),
                raw_opacities.into_primitive().handle.binding(),
            ],
            &[
                arranged_ids.handle.clone().binding(),
                global_from_compact_gid.handle.clone().binding(),
                xys.handle.clone().binding(),
                depths.handle.clone().binding(),
                colors.handle.clone().binding(),
                radii.handle.clone().binding(),
                conic_comps.handle.clone().binding(),
                num_tiles_hit.handle.clone().binding(),
                num_visible.handle.clone().binding(),
            ],
            [num_points as u32],
        );
    }

    let depth_sort_span = SyncSpan::new("DepthSort", device);
    // Interpret the depth as a u32. This is fine for a radix sort, as long as the depth > 0.0,
    // which we know to be the case given how we cull splats.
    let (_, compact_from_depthsort_gid) = radix_argsort(
        bitcast_tensor(depths.clone()),
        arranged_ids,
        num_visible.clone(),
        32,
    );
    drop(depth_sort_span);

    let cum_hit_span = SyncSpan::new("TilesPermute", device);
    // Permute the number of tiles hit for the sorted gaussians.
    // This means num_tiles_hit is not stored per compact_gid, but per depthsort_gid.
    let num_tiles_hit = bitcast_tensor(BurnBack::int_gather(
        0,
        bitcast_tensor(num_tiles_hit),
        bitcast_tensor(compact_from_depthsort_gid.clone()),
    ));

    // Calculate cumulative sums as offsets for the intersections buffer.
    // TODO: Only need to do this up to num_visible gaussians really.
    let cum_tiles_hit = prefix_sum(num_tiles_hit);

    let num_intersects = bitcast_tensor(BurnBack::int_slice(
        bitcast_tensor(cum_tiles_hit.clone()),
        [num_points - 1..num_points],
    ));

    let num_tiles = tile_bounds[0] * tile_bounds[1];

    // TODO: On wasm, we cannot do a sync readback at all.
    // Instead, can just estimate a max number of intersects. All the kernels only handle the actual
    // cound of intersects, and spin up empty threads for the rest atm. In the future, could use indirect
    // dispatch to avoid this.
    // Estimating the max number of intersects can be a bad hack though... The worst case sceneario is so massive
    // that it's easy to run out of memory... How do we actually properly deal with this :/
    let max_intersects = (num_points * (num_tiles as usize)).min(256 * 4 * 65535);
    //let max_intersects =
    //    read_buffer_as_u32(client, num_intersects.clone().handle.binding())[0] as usize;

    // Each intersection maps to a gaussian.
    let tile_id_from_isect = create_tensor::<u32, 1>([max_intersects], device, client);
    let depthsort_gid_from_isect = create_tensor::<u32, 1>([max_intersects], device, client);

    drop(cum_hit_span);

    {
        let _span = SyncSpan::new("MapGaussiansToIntersect", device);

        // Dispatch one thread per point.
        // TODO: Really want to do an indirect dispatch here for num_visible.
        MapGaussiansToIntersect::new().execute(
            client,
            shaders::map_gaussian_to_intersects::Uniforms {
                tile_bounds: tile_bounds.into(),
            },
            &[
                compact_from_depthsort_gid.handle.clone().binding(),
                xys.handle.clone().binding(),
                conic_comps.handle.clone().binding(),
                colors.handle.clone().binding(),
                radii.handle.clone().binding(),
                cum_tiles_hit.handle.clone().binding(),
                num_visible.handle.clone().binding(),
            ],
            &[
                tile_id_from_isect.handle.clone().binding(),
                depthsort_gid_from_isect.handle.clone().binding(),
            ],
            [num_points as u32],
        );
    }

    // We're sorting by tile ID, but we know beforehand what the maximum value
    // can be. We don't need to sort all the leading 0 bits!
    let bits = u32::BITS - num_tiles.leading_zeros();

    let tile_sort_span = SyncSpan::new("Tile sort", device);
    let (tile_id_from_isect, depthsort_gid_from_isect) = radix_argsort(
        tile_id_from_isect,
        depthsort_gid_from_isect,
        num_intersects.clone(),
        bits,
    );
    drop(tile_sort_span);

    let tile_edge_span = SyncSpan::new("GetTileBinEdges", device);
    let tile_bins = BurnBack::int_zeros(
        [tile_bounds.y as usize, tile_bounds.x as usize, 2].into(),
        device,
    );
    GetTileBinEdges::new().execute(
        client,
        (),
        &[
            tile_id_from_isect.handle.binding(),
            num_intersects.handle.clone().binding(),
        ],
        &[tile_bins.handle.clone().binding()],
        [
            (max_intersects as u32).div_ceil(shaders::get_tile_bin_edges::VERTICAL_GROUPS),
            VERTICAL_GROUPS,
            1,
        ],
    );
    drop(tile_edge_span);

    let tile_edge_span = SyncSpan::new("Rasterize", device);

    let out_dim = if raster_u32 {
        // Channels are packed into 4 bytes aka one float.
        1
    } else {
        4
    };

    let out_img = create_tensor(
        [img_size.y as usize, img_size.x as usize, out_dim],
        device,
        client,
    );

    // Record the final visible splat per tile.
    let final_index =
        create_tensor::<u32, 2>([img_size.x as usize, img_size.y as usize], device, client);

    // Only record the final visible splat per tile if we're not rendering a u32 buffer.
    // If we're renering a u32 buffer, we can't autodiff anyway, and final index is only needed for
    // the backward pass.
    let mut out_binds = vec![out_img.handle.clone().binding()];
    if !raster_u32 {
        out_binds.push(final_index.handle.clone().binding());
    }

    Rasterize::new(raster_u32).execute(
        client,
        shaders::rasterize::Uniforms {
            img_size: img_size.into(),
            background: background.xyzx().into(),
            tile_bounds: tile_bounds.into(),
        },
        &[
            depthsort_gid_from_isect.handle.clone().binding(),
            compact_from_depthsort_gid.handle.clone().binding(),
            tile_bins.handle.clone().binding(),
            xys.handle.clone().binding(),
            conic_comps.handle.clone().binding(),
            colors.handle.clone().binding(),
        ],
        out_binds.as_slice(),
        [img_size.x, img_size.y],
    );
    drop(tile_edge_span);

    // TODO: Atm this all still crashes if we're rendering <4 splats due to wgpu
    // limitations on buffer sizes.

    (
        Tensor::from_primitive(out_img),
        RenderAux {
            num_visible: Tensor::from_primitive(bitcast_tensor(num_visible)),
            num_intersects: Tensor::from_primitive(bitcast_tensor(num_intersects)),
            tile_bins: Tensor::from_primitive(bitcast_tensor(tile_bins)),
            cum_tiles_hit: Tensor::from_primitive(bitcast_tensor(cum_tiles_hit)),
            radii_compact: Tensor::from_primitive(bitcast_tensor(radii)),
            conic_comps: Tensor::from_primitive(bitcast_tensor(conic_comps)),
            colors: Tensor::from_primitive(colors),
            depths: Tensor::from_primitive(depths),
            xys: Tensor::from_primitive(bitcast_tensor(xys)),
            final_index: Tensor::from_primitive(bitcast_tensor(final_index)),
            depthsort_gid_from_isect: Tensor::from_primitive(bitcast_tensor(
                depthsort_gid_from_isect,
            )),
            compact_from_depthsort_gid: Tensor::from_primitive(bitcast_tensor(
                compact_from_depthsort_gid,
            )),
            global_from_compact_gid: Tensor::from_primitive(bitcast_tensor(
                global_from_compact_gid,
            )),
        },
    )
}

impl Backend for BurnBack {
    fn render_gaussians(
        camera: &Camera,
        img_size: glam::UVec2,
        means: Tensor<Self, 2>,
        xy_dummy: Tensor<Self, 2>,
        log_scales: Tensor<Self, 2>,
        quats: Tensor<Self, 2>,
        sh_coeffs: Tensor<Self, 2>,
        raw_opacity: Tensor<Self, 1>,
        background: glam::Vec3,
        render_u32_buffer: bool,
    ) -> (Tensor<Self, 3>, RenderAux<BurnBack>) {
        render_forward(
            camera,
            img_size,
            means,
            xy_dummy,
            log_scales,
            quats,
            sh_coeffs,
            raw_opacity,
            background,
            render_u32_buffer,
        )
    }
}

#[derive(Debug, Clone)]
struct GaussianBackwardState {
    cam: Camera,
    background: Vec3,

    // Splat inputs.
    means: NodeID,
    log_scales: NodeID,
    quats: NodeID,
    raw_opac: NodeID,
    sh_degree: usize,
    out_img: Tensor<BurnBack, 3>,
    aux: RenderAux<BurnBack>,
}

#[derive(Debug)]
struct RenderBackwards;

impl<C: CheckpointStrategy> Backend for Autodiff<BurnBack, C> {
    fn render_gaussians(
        camera: &Camera,
        img_size: glam::UVec2,
        means: Tensor<Self, 2>,
        xy_dummy: Tensor<Self, 2>,
        log_scales: Tensor<Self, 2>,
        quats: Tensor<Self, 2>,
        sh_coeffs: Tensor<Self, 2>,
        raw_opacity: Tensor<Self, 1>,
        background: glam::Vec3,
        render_u32_buffer: bool,
    ) -> (Tensor<Self, 3>, RenderAux<Self>) {
        // Prepare backward pass, and check if we even need to do it.
        let prep_nodes = RenderBackwards
            .prepare::<C>([
                means.clone().into_primitive().node,
                xy_dummy.clone().into_primitive().node,
                log_scales.clone().into_primitive().node,
                quats.clone().into_primitive().node,
                sh_coeffs.clone().into_primitive().node,
                raw_opacity.clone().into_primitive().node,
            ])
            .compute_bound()
            .stateful();

        let sh_degree = sh_degree_from_coeffs(sh_coeffs.dims()[1] / 3);

        // Render complete forward pass.
        let (out_img, aux) = render_forward(
            camera,
            img_size,
            means.clone().inner(),
            xy_dummy.clone().inner(),
            log_scales.clone().inner(),
            quats.clone().inner(),
            sh_coeffs.clone().inner(),
            raw_opacity.clone().inner(),
            background,
            render_u32_buffer,
        );

        // Save unwrapped aux for later.
        let orig_aux = aux.clone();

        // Return aux with tensors lifted to the current backend
        // (from the original non autodiff backend).
        let wrap_aux = RenderAux::<Self> {
            num_visible: Tensor::from_inner(aux.num_visible),
            num_intersects: Tensor::from_inner(aux.num_intersects),
            tile_bins: Tensor::from_inner(aux.tile_bins),
            radii_compact: Tensor::from_inner(aux.radii_compact),
            depthsort_gid_from_isect: Tensor::from_inner(aux.depthsort_gid_from_isect),
            compact_from_depthsort_gid: Tensor::from_inner(aux.compact_from_depthsort_gid),
            depths: Tensor::from_inner(aux.depths),
            xys: Tensor::from_inner(aux.xys),
            cum_tiles_hit: Tensor::from_inner(aux.cum_tiles_hit),
            conic_comps: Tensor::from_inner(aux.conic_comps),
            colors: Tensor::from_inner(aux.colors),
            final_index: Tensor::from_inner(aux.final_index),
            global_from_compact_gid: Tensor::from_inner(aux.global_from_compact_gid),
        };

        match prep_nodes {
            OpsKind::Tracked(mut prep) => {
                // Save state needed for backward pass.
                let state = GaussianBackwardState {
                    means: prep.checkpoint(&means.into_primitive()),
                    log_scales: prep.checkpoint(&log_scales.into_primitive()),
                    quats: prep.checkpoint(&quats.into_primitive()),
                    raw_opac: prep.checkpoint(&raw_opacity.into_primitive()),
                    cam: camera.clone(),
                    background,
                    sh_degree,
                    aux: orig_aux,
                    out_img: out_img.clone(),
                };

                (
                    Tensor::from_primitive(prep.finish(state, out_img.into_primitive())),
                    wrap_aux,
                )
            }
            OpsKind::UnTracked(prep) => {
                // When no node is tracked, we can just use the original operation without
                // keeping any state.
                (
                    Tensor::from_primitive(prep.finish(out_img.into_primitive())),
                    wrap_aux,
                )
            }
        }
    }
}

impl Backward<BurnBack, 3, 6> for RenderBackwards {
    type State = GaussianBackwardState;

    fn backward(
        self,
        ops: Ops<Self::State, 6>,
        grads: &mut Gradients,
        checkpointer: &mut Checkpointer,
    ) {
        let _span = info_span!("render_gaussians backwards").entered();

        let state = ops.state;
        let aux = state.aux;

        let img_dimgs = state.out_img.dims();
        let img_size = glam::uvec2(img_dimgs[1] as u32, img_dimgs[0] as u32);
        let tile_bounds = uvec2(
            img_size.x.div_ceil(shaders::rasterize::TILE_WIDTH),
            img_size.y.div_ceil(shaders::rasterize::TILE_WIDTH),
        );

        let v_output = grads.consume::<BurnBack, 3>(&ops.node);
        let client = &v_output.client;
        let device = &v_output.device;

        let means = checkpointer.retrieve_node_output::<FloatTensor<BurnBack, 2>>(state.means);
        let quats = checkpointer.retrieve_node_output::<FloatTensor<BurnBack, 2>>(state.quats);
        let log_scales =
            checkpointer.retrieve_node_output::<FloatTensor<BurnBack, 2>>(state.log_scales);
        let raw_opac =
            checkpointer.retrieve_node_output::<FloatTensor<BurnBack, 1>>(state.raw_opac);

        let num_points = means.shape.dims[0];

        let max_intersects = aux.depthsort_gid_from_isect.shape().dims[0];

        // All the gradients _per tile_. These are later summed up in the final
        // backward projection pass.
        let v_xy_scatter = create_tensor::<f32, 2>([max_intersects, 2], device, client);
        // Nb: this could be 3 floats - but that doesn't play well with wgsl alignment requirements.
        let v_conic_scatter = create_tensor::<f32, 2>([max_intersects, 4], device, client);
        let v_colors_scatter = create_tensor::<f32, 2>([max_intersects, 4], device, client);

        // This is an offset into the scatter tensor buffer. Important to start at zero.
        let hit_ids = BurnBack::int_zeros([num_points].into(), device);

        {
            let _span = SyncSpan::new("RasterizeBackwards", device);

            RasterizeBackwards::new().execute(
                client,
                shaders::rasterize_backwards::Uniforms {
                    img_size: img_size.into(),
                    background: state.background.xyzx().into(),
                    tile_bounds: tile_bounds.into(),
                },
                &[
                    aux.depthsort_gid_from_isect
                        .into_primitive()
                        .handle
                        .binding(),
                    aux.compact_from_depthsort_gid
                        .clone()
                        .into_primitive()
                        .handle
                        .binding(),
                    aux.tile_bins.into_primitive().handle.binding(),
                    aux.xys.into_primitive().handle.binding(),
                    aux.cum_tiles_hit.clone().into_primitive().handle.binding(),
                    aux.conic_comps.clone().into_primitive().handle.binding(),
                    aux.colors.clone().into_primitive().handle.binding(),
                    aux.final_index.into_primitive().handle.binding(),
                    state.out_img.into_primitive().handle.binding(),
                    v_output.handle.binding(),
                ],
                &[
                    v_xy_scatter.handle.clone().binding(),
                    v_conic_scatter.handle.clone().binding(),
                    v_colors_scatter.handle.clone().binding(),
                    hit_ids.handle.clone().binding(),
                ],
                [img_size.x, img_size.y],
            );
        }

        // Create tensors to hold gradients.

        // Nb: these are packed vec3 values, special care is taken in the kernel to respect alignment.
        // Nb: These have to be zerod out - as we only write to visible splats.
        let v_means = BurnBack::float_zeros([num_points, 3].into(), device);
        let v_scales = BurnBack::float_zeros([num_points, 3].into(), device);
        let v_quats = BurnBack::float_zeros([num_points, 4].into(), device);
        let v_coeffs = BurnBack::float_zeros(
            [num_points, num_sh_coeffs(state.sh_degree) * 3].into(),
            device,
        );
        let v_opac = BurnBack::float_zeros([num_points].into(), device);
        let v_xys = BurnBack::float_zeros([num_points, 2].into(), device);

        {
            let _span = SyncSpan::new("ProjectBackwards", device);

            ProjectBackwards::new().execute(
                client,
                shaders::project_backwards::Uniforms {
                    viewmat: state.cam.world_to_local().to_cols_array_2d(),
                    focal: state.cam.focal(img_size).into(),
                    img_size: img_size.into(),
                    sh_degree_pad: glam::UVec4::new(state.sh_degree as u32, 0, 0, 0).into(),
                },
                &[
                    means.handle.binding(),
                    log_scales.handle.binding(),
                    quats.handle.binding(),
                    raw_opac.handle.binding(),
                    aux.conic_comps.into_primitive().handle.binding(),
                    aux.cum_tiles_hit.into_primitive().handle.clone().binding(),
                    v_xy_scatter.handle.binding(),
                    v_conic_scatter.handle.binding(),
                    v_colors_scatter.handle.binding(),
                    aux.num_visible.into_primitive().handle.clone().binding(),
                    aux.global_from_compact_gid
                        .into_primitive()
                        .handle
                        .clone()
                        .binding(),
                    aux.compact_from_depthsort_gid
                        .clone()
                        .into_primitive()
                        .handle
                        .clone()
                        .binding(),
                ],
                &[
                    v_means.handle.clone().binding(),
                    v_xys.handle.clone().binding(),
                    v_scales.handle.clone().binding(),
                    v_quats.handle.clone().binding(),
                    v_coeffs.handle.clone().binding(),
                    v_opac.handle.clone().binding(),
                ],
                [num_points as u32],
            );
        }

        // Register gradients for parent nodes (This code is already skipped entirely
        // if no parent nodes require gradients).
        let [mean_parent, xys_parent, log_scales_parent, quats_parent, coeffs_parent, raw_opacity_parent] =
            ops.parents;

        if let Some(node) = mean_parent {
            grads.register::<BurnBack, 2>(node.id, v_means);
        }

        // Register the gradients for the dummy xy input.
        if let Some(node) = xys_parent {
            grads.register::<BurnBack, 2>(node.id, v_xys);
        }

        if let Some(node) = log_scales_parent {
            grads.register::<BurnBack, 2>(node.id, v_scales);
        }

        if let Some(node) = quats_parent {
            grads.register::<BurnBack, 2>(node.id, v_quats);
        }

        if let Some(node) = coeffs_parent {
            grads.register::<BurnBack, 2>(node.id, v_coeffs);
        }

        if let Some(node) = raw_opacity_parent {
            grads.register::<BurnBack, 1>(node.id, v_opac);
        }
    }
}

pub fn render<B: Backend>(
    camera: &Camera,
    img_size: glam::UVec2,
    means: Tensor<B, 2>,
    xy_dummy: Tensor<B, 2>,
    log_scales: Tensor<B, 2>,
    quats: Tensor<B, 2>,
    sh_coeffs: Tensor<B, 2>,
    raw_opacity: Tensor<B, 1>,
    background: glam::Vec3,
    render_u32_buffer: bool,
) -> (Tensor<B, 3>, RenderAux<B>) {
    let (img, aux) = B::render_gaussians(
        camera,
        img_size,
        means,
        xy_dummy,
        log_scales,
        quats,
        sh_coeffs,
        raw_opacity,
        background,
        render_u32_buffer,
    );
    (img, aux)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::fs::File;
    use std::io::Read;

    use crate::camera::{focal_to_fov, fov_to_focal};

    use super::*;
    use assert_approx_eq::assert_approx_eq;
    use burn::tensor::{Data, Float};
    use burn_wgpu::WgpuDevice;

    type DiffBack = Autodiff<BurnBack>;

    use safetensors::tensor::TensorView;
    use safetensors::SafeTensors;

    use anyhow::{Context, Result};

    #[test]
    fn renders_at_all() {
        // Check if rendering doesn't hard crash or anything.
        // These are some zero-sized gaussians, so we know
        // what the result should look like.
        let cam = Camera::new(glam::vec3(0.0, 0.0, 0.0), glam::Quat::IDENTITY, 0.5, 0.5);
        let img_size = glam::uvec2(32, 32);
        let device = WgpuDevice::BestAvailable;

        let num_points = 8;
        let means = Tensor::<DiffBack, 2, _>::zeros([num_points, 3], &device);
        let xy_dummy = Tensor::<DiffBack, 2, _>::zeros([num_points, 2], &device);

        let log_scales = Tensor::ones([num_points, 3], &device) * 2.0;
        let quats = Tensor::from_data(glam::Quat::IDENTITY.to_array(), &device)
            .unsqueeze_dim(0)
            .repeat(0, num_points);
        let sh_coeffs = Tensor::ones([num_points, 4], &device);
        let raw_opacity = Tensor::zeros([num_points], &device);
        let (output, _) = render(
            &cam,
            img_size,
            means,
            xy_dummy,
            log_scales,
            quats,
            sh_coeffs,
            raw_opacity,
            glam::vec3(0.123, 0.123, 0.123),
            false,
        );

        let rgb = output.clone().slice([0..32, 0..32, 0..3]);
        let alpha = output.clone().slice([0..32, 0..32, 3..4]);
        // TODO: Maybe use all_close from burn - but that seems to be
        // broken atm.
        assert_approx_eq!(rgb.clone().mean().to_data().value[0], 0.123, 1e-5);
        assert_approx_eq!(alpha.clone().mean().to_data().value[0], 0.0);
    }

    fn float_from_u8(data: &[u8]) -> Vec<f32> {
        data.chunks_exact(4)
            .map(|x| f32::from_le_bytes([x[0], x[1], x[2], x[3]]))
            .collect()
    }

    // Nb: this only handles float tensors, good enough :)
    fn safe_tensor_to_burn1<B: Backend>(t: TensorView, device: &B::Device) -> Tensor<B, 1, Float> {
        Tensor::from_data(
            Data::new(float_from_u8(t.data()), [t.shape()[0]].into()).convert(),
            device,
        )
    }

    fn safe_tensor_to_burn2<B: Backend>(t: TensorView, device: &B::Device) -> Tensor<B, 2, Float> {
        Tensor::from_data(
            Data::new(float_from_u8(t.data()), [t.shape()[0], t.shape()[1]].into()).convert(),
            device,
        )
    }

    fn safe_tensor_to_burn3<B: Backend>(t: TensorView, device: &B::Device) -> Tensor<B, 3, Float> {
        Tensor::from_data(
            Data::new(
                float_from_u8(t.data()),
                [t.shape()[0], t.shape()[1], t.shape()[2]].into(),
            )
            .convert(),
            device,
        )
    }

    #[test]
    fn test_reference() -> Result<()> {
        let device = WgpuDevice::BestAvailable;
        #[cfg(feature = "rerun")]
        let rec = rerun::RecordingStreamBuilder::new("visualize training").spawn()?;

        let crab_img = image::open("./test_cases/crab.png")?;
        // Convert the image to RGB format
        // Get the raw buffer
        let raw_buffer = crab_img.to_rgb8().into_raw();
        let crab_tens: Tensor<DiffBack, 3> = Tensor::from_floats(
            raw_buffer
                .iter()
                .map(|&b| b as f32 / 255.0)
                .collect::<Vec<_>>()
                .as_slice(),
            &device,
        )
        .reshape([crab_img.height() as usize, crab_img.width() as usize, 3]);

        for path in ["basic_case", "mix_case"] {
            let mut buffer = Vec::new();
            let _ =
                File::open(format!("./test_cases/{path}.safetensors"))?.read_to_end(&mut buffer)?;
            let tensors = SafeTensors::deserialize(&buffer)?;

            let means =
                safe_tensor_to_burn2::<DiffBack>(tensors.tensor("means")?, &device).require_grad();
            let num_points = means.dims()[0];

            let xy_dummy = Tensor::zeros([num_points, 2], &device).require_grad();

            let log_scales =
                safe_tensor_to_burn2::<DiffBack>(tensors.tensor("scales")?, &device).require_grad();

            let coeffs = safe_tensor_to_burn3::<DiffBack>(tensors.tensor("coeffs")?, &device)
                .reshape([num_points, 3])
                .require_grad();

            let quats =
                safe_tensor_to_burn2::<DiffBack>(tensors.tensor("quats")?, &device).require_grad();
            let opacities = safe_tensor_to_burn1::<DiffBack>(tensors.tensor("opacities")?, &device)
                .require_grad();

            let xys_ref = safe_tensor_to_burn2::<DiffBack>(tensors.tensor("xys")?, &device);
            let conics_ref = safe_tensor_to_burn2::<DiffBack>(tensors.tensor("conics")?, &device);
            let img_ref = safe_tensor_to_burn3::<DiffBack>(tensors.tensor("out_img")?, &device);
            let img_dims = img_ref.dims();

            let fov = std::f32::consts::PI * 0.5;

            let focal = fov_to_focal(fov, img_dims[1] as u32);
            let fov_x = focal_to_fov(focal, img_dims[1] as u32);
            let fov_y = focal_to_fov(focal, img_dims[0] as u32);

            let cam = Camera::new(
                glam::vec3(0.0, 0.0, -8.0),
                glam::Quat::IDENTITY,
                fov_x,
                fov_y,
            );

            let (out, aux) = render(
                &cam,
                glam::uvec2(img_dims[1] as u32, img_dims[0] as u32),
                means.clone(),
                xy_dummy.clone(),
                log_scales.clone(),
                quats.clone(),
                coeffs.clone(),
                opacities.clone(),
                glam::vec3(0.0, 0.0, 0.0),
                false,
            );

            let out_rgb = out.clone().slice([0..img_dims[0], 0..img_dims[1], 0..3]);

            #[cfg(feature = "rerun")]
            {
                use ndarray::Array;
                rec.log(
                    "img/image",
                    &rerun::Image::try_from(
                        Array::from_shape_vec(out_rgb.dims(), out_rgb.to_data().value)?
                            .map(|x| (*x * 255.0).clamp(0.0, 255.0) as u8),
                    )?,
                )?;

                rec.log(
                    "img/ref",
                    &rerun::Image::try_from(
                        Array::from_shape_vec(img_ref.dims(), img_ref.to_data().value)?
                            .map(|x| (*x * 255.0).clamp(0.0, 255.0) as u8),
                    )?,
                )?;

                rec.log(
                    "img/dif",
                    &rerun::Tensor::try_from(Array::from_shape_vec(
                        img_ref.dims(),
                        (img_ref.clone() - out_rgb.clone()).to_data().value,
                    )?)?,
                )?;

                let tile_depth = aux.calc_tile_depth();
                rec.log(
                    "images/tile depth",
                    &rerun::Tensor::try_from(Array::from_shape_vec(
                        tile_depth.dims(),
                        tile_depth.to_data().convert::<i32>().value,
                    )?)?,
                )?;
            }

            let num_visible = aux.num_visible.to_data().value[0] as usize;
            let perm = aux.global_from_compact_gid.clone();

            let xys = aux.xys.slice([0..num_visible]);
            let xys_ref = xys_ref.select(0, perm.clone()).slice([0..num_visible]);

            let conics = aux.conic_comps.slice([0..num_visible, 0..3]);
            let conics_ref = conics_ref.select(0, perm.clone()).slice([0..num_visible]);

            let grads = (out_rgb.clone() - crab_tens.clone())
                .powf_scalar(2.0)
                .mean()
                .backward();

            let v_opacities_ref =
                safe_tensor_to_burn1::<DiffBack>(tensors.tensor("v_opacities")?, &device).inner();
            let v_opacities = opacities.grad(&grads).context("opacities grad")?;

            let v_coeffs_ref =
                safe_tensor_to_burn3::<DiffBack>(tensors.tensor("v_coeffs")?, &device)
                    .reshape([num_points, 3])
                    .inner();
            let v_coeffs = coeffs.grad(&grads).context("coeffs grad")?;

            let v_quats = quats.grad(&grads).context("quats grad")?;
            let v_quats_ref =
                safe_tensor_to_burn2::<DiffBack>(tensors.tensor("v_quats")?, &device).inner();

            let v_scales = log_scales.grad(&grads).context("scales grad")?;
            let v_scales_ref =
                safe_tensor_to_burn2::<DiffBack>(tensors.tensor("v_scales")?, &device).inner();

            let v_means_ref =
                safe_tensor_to_burn2::<DiffBack>(tensors.tensor("v_means")?, &device).inner();
            let v_means = means.grad(&grads).context("means grad")?;

            let v_xys_ref =
                safe_tensor_to_burn2::<DiffBack>(tensors.tensor("v_xy")?, &device).inner();

            let v_xys = xy_dummy.grad(&grads).context("no xys grad")?;

            assert!(xys.all_close(xys_ref, Some(1e-5), Some(1e-5)));
            assert!(conics.all_close(conics_ref, Some(1e-5), Some(1e-6)));
            assert!(out_rgb.all_close(img_ref, Some(1e-5), Some(1e-6)));

            assert!(v_xys.all_close(v_xys_ref, Some(1e-5), Some(1e-6)));
            assert!(v_opacities.all_close(v_opacities_ref, Some(1e-5), Some(1e-6)));
            assert!(v_coeffs.all_close(v_coeffs_ref, Some(1e-5), Some(1e-6)));
            assert!(v_quats.all_close(v_quats_ref, Some(1e-5), Some(1e-6)));
            assert!(v_scales.all_close(v_scales_ref, Some(1e-5), Some(1e-6)));
            assert!(v_means.all_close(v_means_ref, Some(1e-5), Some(1e-6)));
        }
        Ok(())
    }

    // #[test]
    // fn test_mean_grads() {
    //     let cam = Camera::new(glam::vec3(0.0, 0.0, -5.0), glam::Quat::IDENTITY, 0.5, 0.5);
    //     let num_points = 1;

    //     let img_size = glam::uvec2(16, 16);
    //     let device = WgpuDevice::BestAvailable;

    //     let means = Tensor::<DiffBack, 2, _>::zeros([num_points, 3], &device).require_grad();
    //     let log_scales = Tensor::ones([num_points, 3], &device).require_grad();
    //     let quats = Tensor::from_data(glam::Quat::IDENTITY.to_array(), &device)
    //         .unsqueeze_dim(0)
    //         .repeat(0, num_points)
    //         .require_grad();
    //     let sh_coeffs = Tensor::zeros([num_points, 4], &device).require_grad();
    //     let raw_opacity = Tensor::zeros([num_points], &device).require_grad();

    //     let mut dloss_dmeans_stat = Tensor::zeros([num_points], &device);

    //     // Calculate a stochasic gradient estimation by perturbing random dimensions.
    //     let num_iters = 50;

    //     for _ in 0..num_iters {
    //         let eps = 1e-4;

    //         let flip_vec = Tensor::<DiffBack, 1>::random(
    //             [num_points],
    //             burn::tensor::Distribution::Uniform(-1.0, 1.0),
    //             &device,
    //         );
    //         let seps = flip_vec * eps;

    //         let l1 = render(
    //             &cam,
    //             img_size,
    //             means.clone(),
    //             log_scales.clone(),
    //             quats.clone(),
    //             sh_coeffs.clone(),
    //             raw_opacity.clone() - seps.clone(),
    //             glam::Vec3::ZERO,
    //         )
    //         .0
    //         .mean();

    //         let l2 = render(
    //             &cam,
    //             img_size,
    //             means.clone(),
    //             log_scales.clone(),
    //             quats.clone(),
    //             sh_coeffs.clone(),
    //             raw_opacity.clone() + seps.clone(),
    //             glam::Vec3::ZERO,
    //         )
    //         .0
    //         .mean();

    //         let df = l2 - l1;
    //         dloss_dmeans_stat = dloss_dmeans_stat + df * (seps * 2.0).recip();
    //     }

    //     dloss_dmeans_stat = dloss_dmeans_stat / (num_iters as f32);
    //     let dloss_dmeans_stat = dloss_dmeans_stat.into_data().value;

    //     let loss = render(
    //         &cam,
    //         img_size,
    //         means.clone(),
    //         log_scales.clone(),
    //         quats.clone(),
    //         sh_coeffs.clone(),
    //         raw_opacity.clone(),
    //         glam::Vec3::ZERO,
    //     )
    //     .0
    //     .mean();
    //     // calculate numerical gradients.
    //     // Compare to reference value.

    //     // Check if rendering doesn't hard crash or anything.
    //     // These are some zero-sized gaussians, so we know
    //     // what the result should look like.
    //     let grads = loss.backward();

    //     // Get the gradient of the rendered image.
    //     let dloss_dmeans = (Tensor::<BurnBack, 1>::from_primitive(
    //         grads.get(&raw_opacity.clone().into_primitive()).unwrap(),
    //     ))
    //     .into_data()
    //     .value;

    //     println!("Stat grads {dloss_dmeans_stat:.5?}");
    //     println!("Calc grads {dloss_dmeans:.5?}");

    //     // TODO: These results don't make sense at all currently, which is either
    //     // mildly bad news or very bad news :)
    // }
}