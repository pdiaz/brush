use brush_render::RenderAux;
use brush_render::{gaussian_splats::Splats, Backend};
use burn::tensor::{ElementConversion, Tensor};
use rand::seq::IteratorRandom;

use crate::image::view_to_sample;
use crate::scene::{Scene, SceneView};
use crate::ssim::Ssim;

pub struct EvalView<B: Backend> {
    pub view: SceneView,
    pub rendered: Tensor<B, 3>,
    // TODO: Maybe these are better kept as tensors too,
    // but would complicate displaying things in the stats panel a bit.
    pub psnr: f32,
    pub ssim: f32,
    pub aux: RenderAux<B>,
}

pub struct EvalStats<B: Backend> {
    pub samples: Vec<EvalView<B>>,
}

impl<B: Backend> EvalStats<B> {
    /// Calculate the average PSNR of all samples.
    pub fn avg_psnr(&self) -> f32 {
        self.samples.iter().map(|s| s.psnr).sum::<f32>() / (self.samples.len() as f32)
    }

    /// Calculate the average PSNR of all samples.
    pub fn avg_ssim(&self) -> f32 {
        self.samples.iter().map(|s| s.ssim).sum::<f32>() / (self.samples.len() as f32)
    }
}

pub async fn eval_stats<B: Backend>(
    splats: Splats<B>,
    eval_scene: &Scene,
    num_frames: Option<usize>,
    rng: &mut impl rand::Rng,
    device: &B::Device,
) -> EvalStats<B> {
    let indices = if let Some(num) = num_frames {
        (0..eval_scene.views.len()).choose_multiple(rng, num)
    } else {
        (0..eval_scene.views.len()).collect()
    };

    let eval_views: Vec<_> = indices
        .into_iter()
        .map(|i| eval_scene.views[i].clone())
        .collect();

    let mut ret = vec![];

    for view in eval_views {
        // Compare MSE in RGB only, not sure if this should include alpha.
        let res = glam::uvec2(view.image.width(), view.image.height());

        let gt_tensor = view_to_sample::<B>(&view, device);
        let gt_rgb = gt_tensor.slice([0..res.y as usize, 0..res.x as usize, 0..3]);

        let (rendered, aux) = splats.render(&view.camera, res, false);

        let render_rgb = rendered.slice([0..res.y as usize, 0..res.x as usize, 0..3]);
        let mse = (render_rgb.clone() - gt_rgb.clone())
            .powf_scalar(2.0)
            .mean();

        let psnr = mse.recip().log() * 10.0 / std::f32::consts::LN_10;
        let psnr = psnr.into_scalar_async().await.elem::<f32>();

        let ssim_measure = Ssim::new(11, 3, device);
        let ssim = ssim_measure
            .ssim(render_rgb.clone().unsqueeze(), gt_rgb.unsqueeze())
            .mean();
        let ssim = ssim.into_scalar_async().await.elem::<f32>();

        ret.push(EvalView {
            view,
            psnr,
            ssim,
            rendered: render_rgb,
            aux,
        });
    }

    EvalStats { samples: ret }
}
