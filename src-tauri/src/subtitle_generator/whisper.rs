use async_trait::async_trait;

use crate::progress_event::ProgressReporterTrait;
use async_std::sync::{Arc, RwLock};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use super::SubtitleGenerator;

#[derive(Clone)]
pub struct WhisperCPP {
    ctx: Arc<RwLock<WhisperContext>>,
    prompt: String,
}

pub async fn new(model: &Path, prompt: &str) -> Result<WhisperCPP, String> {
    let ctx = WhisperContext::new_with_params(
        model.to_str().unwrap(),
        WhisperContextParameters::default(),
    )
    .expect("failed to load model");

    Ok(WhisperCPP {
        ctx: Arc::new(RwLock::new(ctx)),
        prompt: prompt.to_string(),
    })
}

#[async_trait]
impl SubtitleGenerator for WhisperCPP {
    async fn generate_subtitle(
        &self,
        reporter: &impl ProgressReporterTrait,
        audio_path: &Path,
        output_path: &Path,
    ) -> Result<String, String> {
        log::info!("Generating subtitle for {:?}", audio_path);
        let start_time = std::time::Instant::now();
        let audio = hound::WavReader::open(audio_path).map_err(|e| e.to_string())?;
        let samples: Vec<i16> = audio.into_samples::<i16>().map(|x| x.unwrap()).collect();

        let state = self.ctx.read().await.create_state();

        if let Err(e) = state {
            return Err(e.to_string());
        }

        let mut state = state.unwrap();

        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });

        // and set the language to translate to to auto
        params.set_language(None);
        params.set_initial_prompt(self.prompt.as_str());

        // we also explicitly disable anything that prints to stdout
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        params.set_progress_callback_safe(move |p| {
            log::info!("Progress: {}%", p);
        });

        let mut inter_samples = vec![Default::default(); samples.len()];

        reporter.update("处理音频中");
        if let Err(e) = whisper_rs::convert_integer_to_float_audio(&samples, &mut inter_samples) {
            return Err(e.to_string());
        }

        let samples = whisper_rs::convert_stereo_to_mono_audio(&inter_samples);
        if let Err(e) = samples {
            return Err(e.to_string());
        }

        let samples = samples.unwrap();

        reporter.update("生成字幕中");
        if let Err(e) = state.full(params, &samples[..]) {
            log::error!("failed to run model: {}", e);
            return Err(e.to_string());
        }

        // open the output file
        let mut output_file = tokio::fs::File::create(output_path).await.map_err(|e| {
            log::error!("failed to create output file: {}", e);
            e.to_string()
        })?;
        // fetch the results
        let num_segments = state.full_n_segments().map_err(|e| e.to_string())?;
        let mut subtitle = String::new();
        for i in 0..num_segments {
            let segment = state.full_get_segment_text(i).map_err(|e| e.to_string())?;
            let start_timestamp = state.full_get_segment_t0(i).map_err(|e| e.to_string())?;
            let end_timestamp = state.full_get_segment_t1(i).map_err(|e| e.to_string())?;

            let format_time = |timestamp: f64| {
                let hours = (timestamp / 3600.0).floor();
                let minutes = ((timestamp - hours * 3600.0) / 60.0).floor();
                let seconds = timestamp - hours * 3600.0 - minutes * 60.0;
                format!("{:02}:{:02}:{:06.3}", hours, minutes, seconds).replace(".", ",")
            };

            let line = format!(
                "{}\n{} --> {}\n{}\n\n",
                i + 1,
                format_time(start_timestamp as f64 / 100.0),
                format_time(end_timestamp as f64 / 100.0),
                segment,
            );

            subtitle.push_str(&line);
        }

        output_file
            .write_all(subtitle.as_bytes())
            .await
            .expect("failed to write to output file");

        log::info!("Subtitle generated: {:?}", output_path);
        log::info!("Time taken: {} seconds", start_time.elapsed().as_secs_f64());

        Ok(subtitle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "need whisper-cli"]
    async fn create_whisper_cpp() {
        let result = new(Path::new("tests/model/ggml-model-whisper-tiny.bin"), "").await;
        assert!(result.is_ok());
    }
}
