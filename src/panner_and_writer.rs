use std::{
    collections::VecDeque,
    f64::consts::PI,
    io::Result,
    sync::{Arc, Mutex},
};

pub const LFE_START: f64 = 40.0;
const LFE_FULL: f64 = 20.0;
const HALF_PI: f64 = PI / 2.0;

use rustfft::{num_complex::Complex, Fft};
use wave_stream::{samples_by_channel::SamplesByChannel, wave_writer::RandomAccessWavWriter};

use crate::{
    matrix,
    options::{db_to_amplitude, Options},
    structs::{ThreadState, TransformedWindowAndPans},
    upmixer::Upmixer,
};

pub struct PannerAndWriter {
    // A queue of transformed windows and all of the panned locations of each frequency, after averaging
    transformed_window_and_averaged_pans_queue: Mutex<VecDeque<TransformedWindowAndPans>>,

    // Wav writer and state used to communicate status
    writer_state: Mutex<WriterState>,

    fft_inverse: Arc<dyn Fft<f64>>,

    lfe_levels: Option<Vec<f64>>,

    max_samples_in_file: usize,
}

// Wraps types used during writing so they can be within a mutex
struct WriterState {
    pub target_random_access_wav_writers: Vec<RandomAccessWavWriter<f32>>,
    pub total_samples_written: usize,
}

impl PannerAndWriter {
    pub fn new(
        options: &Options,
        window_size: usize,
        sample_rate: usize,
        target_random_access_wav_writers: Vec<RandomAccessWavWriter<f32>>,
        fft_inverse: Arc<dyn Fft<f64>>,
        max_samples_in_file: usize,
    ) -> PannerAndWriter {
        let lfe_levels = if options.channels.low_frequency {
            let mut lfe_levels = vec![0.0f64; window_size];
            let window_midpoint = window_size / 2;

            let sample_rate_f64 = sample_rate as f64;
            let window_size_f64 = window_size as f64;

            lfe_levels[0] = 1.0;
            lfe_levels[window_midpoint] = 0.0;

            // Calculate ranges for averaging each sub frequency
            for transform_index in 1..(window_midpoint - 2) {
                let transform_index_f64 = transform_index as f64;
                // Out of 8
                // 1, 2, 3, 4
                // 8, 4, 2, 1
                let wavelength = window_size_f64 / transform_index_f64;
                let frequency = sample_rate_f64 / wavelength;

                let level = if frequency < LFE_FULL {
                    1.0
                } else if frequency < LFE_START {
                    let frequency_fraction = (frequency - LFE_FULL) / LFE_FULL;
                    (frequency_fraction * HALF_PI).cos()
                } else {
                    0.0
                };

                lfe_levels[transform_index] = level;
                lfe_levels[window_size - transform_index] = level;
            }

            Some(lfe_levels)
        } else {
            None
        };

        PannerAndWriter {
            transformed_window_and_averaged_pans_queue: Mutex::new(VecDeque::new()),
            writer_state: Mutex::new(WriterState {
                target_random_access_wav_writers,
                total_samples_written: 0,
            }),
            fft_inverse,
            lfe_levels,
            max_samples_in_file,
        }
    }

    pub fn get_inplace_scratch_len(self: &PannerAndWriter) -> usize {
        self.fft_inverse.get_inplace_scratch_len()
    }

    pub fn get_total_samples_written(self: &PannerAndWriter) -> usize {
        self.writer_state
            .lock()
            .expect("Cannot aquire lock because a thread panicked")
            .total_samples_written
    }

    pub fn enqueue(self: &PannerAndWriter, transformed_window_and_pans: TransformedWindowAndPans) {
        self.transformed_window_and_averaged_pans_queue
            .lock()
            .expect("Cannot aquire lock because a thread panicked")
            .push_back(transformed_window_and_pans);
    }

    pub fn perform_backwards_transform_and_write_samples(
        self: &PannerAndWriter,
        thread_state: &mut ThreadState,
    ) -> Result<()> {
        'transform_and_write: loop {
            let transformed_window_and_pans = {
                let mut transformed_window_and_averaged_pans_queue = self
                    .transformed_window_and_averaged_pans_queue
                    .lock()
                    .expect("Cannot aquire lock because a thread panicked");

                match transformed_window_and_averaged_pans_queue.pop_front() {
                    Some(transformed_window_and_pans) => transformed_window_and_pans,
                    None => {
                        break 'transform_and_write;
                    }
                }
            };

            // The front channels are based on the original transforms
            let mut left_front = transformed_window_and_pans
                .left_transformed
                .expect("Transform expected, got a placeholder instead");
            let mut right_front = transformed_window_and_pans
                .right_transformed
                .expect("Transform expected, got a placeholder instead");

            // Rear channels start as copies of the front channels
            let mut left_rear = left_front.clone();
            let mut right_rear = right_front.clone();

            let lfe = if thread_state.upmixer.options.channels.low_frequency {
                transformed_window_and_pans.mono_transformed.clone()
            } else {
                None
            };

            let mut center = if thread_state.upmixer.options.channels.front_center {
                transformed_window_and_pans.mono_transformed
            } else {
                None
            };

            // Ultra-lows are not shitfted
            left_rear[0] = Complex { re: 0f64, im: 0f64 };
            right_rear[0] = Complex { re: 0f64, im: 0f64 };

            // Steer each frequency
            for freq_ctr in 1..(thread_state.upmixer.window_midpoint + 1) {
                // Phase is offset from sine/cos in # of samples
                let left = left_front[freq_ctr];
                let (left_amplitude, mut left_front_phase) = left.to_polar();
                let right = right_front[freq_ctr];
                let (right_amplitude, mut right_front_phase) = right.to_polar();

                let mut left_rear_phase = left_front_phase;
                let mut right_rear_phase = right_front_phase;

                let frequency_pans = &transformed_window_and_pans.frequency_pans[freq_ctr - 1];
                let left_to_right = frequency_pans.left_to_right;
                let back_to_front = frequency_pans.back_to_front;

                // Widening is currently disabled because it results in poor audio quality, and favors too
                // much steering to the rear
                //thread_state.upmixer.options.matrix.widen(&mut back_to_front, &mut left_to_right);

                let front_to_back = 1f64 - back_to_front;

                // Figure out the amplitudes for front and rear
                let mut left_front_amplitude: f64;
                let left_rear_amplitude: f64;
                let mut right_front_amplitude: f64;
                let right_rear_amplitude: f64;

                // sq requires oddbal adjustment of right-left panning
                if thread_state.upmixer.options.matrix.steer_right_left() {
                    // 0.0 is left, 1.0 is right
                    let left_to_right_no_center = (left_to_right / 2.0) + 0.5;

                    // lower amplitude when a tone is between the front and back
                    // When a tone is centered between two speakers, it is lowered by .707 so it's just as loud as when it's isolated in the speaker
                    let isolated_in_front_or_back = ((front_to_back * 2.0) - 1.0).abs();
                    let panned_between_front_or_back = 1.0 - isolated_in_front_or_back;
                    let amplitude = (frequency_pans.amplitude * isolated_in_front_or_back)
                        + (frequency_pans.amplitude
                            * panned_between_front_or_back
                            * matrix::CENTER_AMPLITUDE_ADJUSTMENT);

                    let amplitude = if thread_state.upmixer.options.loud {
                        amplitude
                    } else {
                        amplitude * thread_state.upmixer.options.matrix.amplitude_adjustment()
                    };

                    let amplitude_front = amplitude * front_to_back;

                    // Steer center
                    let front_side_adjustment = left_to_right.abs();
                    let front_center_adjustment = 1.0 - front_side_adjustment;
                    center = match center {
                        Some(mut center) => {
                            // Uncomment to set breakpoints
                            /*if transformed_window_and_pans.last_sample_ctr == 17640 && freq_ctr == 46 {
                                print!("");
                            }*/

                            let center_amplitude: f64;
                            // Adjust the left and right channels
                            if left_to_right == 0.0 {
                                // Frequency is center-panned
                                left_front_amplitude = 0.0;
                                right_front_amplitude = 0.0;
                                center_amplitude = amplitude_front;
                            } else {
                                // Adjust by .707 for tones off-center
                                let front_side_adjustment =
                                    ((front_side_adjustment * 2.0) - 1.0).abs();
                                let front_center_adjustment = 1.0 - front_side_adjustment;
                                let amplitude_mix_front = (amplitude_front * front_side_adjustment)
                                    + (amplitude_front
                                        * front_center_adjustment
                                        * matrix::CENTER_AMPLITUDE_ADJUSTMENT);

                                center_amplitude = amplitude_mix_front * front_center_adjustment;

                                if left_to_right < 0.0 {
                                    // Frequency is left-panned
                                    left_front_amplitude =
                                        amplitude_mix_front * front_side_adjustment;
                                    right_front_amplitude = 0.0;
                                } else {
                                    //if left_to_right > 0.0 {
                                    // Frequency is right-panned
                                    left_front_amplitude = 0.0;
                                    right_front_amplitude =
                                        amplitude_mix_front * front_side_adjustment;
                                }
                            }

                            let (_, phase) = center[freq_ctr].to_polar();
                            let c = Complex::from_polar(center_amplitude, phase);

                            center[freq_ctr] = c;
                            if freq_ctr < thread_state.upmixer.window_midpoint {
                                center[thread_state.upmixer.window_size - freq_ctr] = Complex {
                                    re: c.re,
                                    im: -1.0 * c.im,
                                }
                            }

                            Some(center)
                        }
                        None => {
                            // Adjust by .707 for centered tones
                            let amplitude_mix_front = (amplitude_front * front_side_adjustment)
                                + (amplitude_front
                                    * front_center_adjustment
                                    * matrix::CENTER_AMPLITUDE_ADJUSTMENT);

                            right_front_amplitude = amplitude_mix_front * left_to_right_no_center;
                            left_front_amplitude = amplitude_mix_front - right_front_amplitude;
                            None
                        }
                    };

                    // The back pans also need to be adjusted by left_to_right, because SQ's left-right panning is phase-based
                    let amplitude_back = amplitude * back_to_front;
                    right_rear_amplitude = amplitude_back * left_to_right_no_center;
                    left_rear_amplitude = amplitude_back - right_rear_amplitude;
                } else {
                    // normal matrixes don't adjust left <-> right
                    let amplitude_adjustment = if thread_state.upmixer.options.loud {
                        thread_state.upmixer.options.matrix.amplitude_adjustment()
                    } else {
                        1.0f64
                    };

                    let left_amplitude = left_amplitude / amplitude_adjustment;
                    let right_amplitude = right_amplitude / amplitude_adjustment;

                    // Figure out the amplitudes for front and rear
                    left_front_amplitude = left_amplitude * front_to_back;
                    right_front_amplitude = right_amplitude * front_to_back;
                    left_rear_amplitude = left_amplitude * back_to_front;
                    right_rear_amplitude = right_amplitude * back_to_front;

                    // Steer center
                    center = match center {
                        Some(mut center) => {
                            let (_, phase) = center[freq_ctr].to_polar();
                            let center_amplitude = (1.0 - left_to_right.abs())
                                * (left_front_amplitude + right_front_amplitude)
                                * matrix::CENTER_AMPLITUDE_ADJUSTMENT
                                * 0.5;
                            let c = Complex::from_polar(center_amplitude, phase);

                            center[freq_ctr] = c;
                            if freq_ctr < thread_state.upmixer.window_midpoint {
                                center[thread_state.upmixer.window_size - freq_ctr] = Complex {
                                    re: c.re,
                                    im: -1.0 * c.im,
                                }
                            }

                            // Subtract the center from the right and left front channels
                            left_front_amplitude =
                                f64::max(0.0, left_front_amplitude - center_amplitude);
                            right_front_amplitude =
                                f64::max(0.0, right_front_amplitude - center_amplitude);

                            Some(center)
                        }
                        None => None,
                    };
                }

                // Phase shifts
                thread_state.upmixer.options.matrix.phase_shift(
                    &mut left_front_phase,
                    &mut right_front_phase,
                    &mut left_rear_phase,
                    &mut right_rear_phase,
                );

                // Assign to array
                left_front[freq_ctr] = Complex::from_polar(left_front_amplitude, left_front_phase);
                right_front[freq_ctr] =
                    Complex::from_polar(right_front_amplitude, right_front_phase);
                left_rear[freq_ctr] = Complex::from_polar(left_rear_amplitude, left_rear_phase);
                right_rear[freq_ctr] = Complex::from_polar(right_rear_amplitude, right_rear_phase);

                if freq_ctr < thread_state.upmixer.window_midpoint {
                    let inverse_freq_ctr = thread_state.upmixer.window_size - freq_ctr;
                    left_front[inverse_freq_ctr] = Complex {
                        re: left_front[freq_ctr].re,
                        im: -1.0 * left_front[freq_ctr].im,
                    };
                    right_front[inverse_freq_ctr] = Complex {
                        re: right_front[freq_ctr].re,
                        im: -1.0 * right_front[freq_ctr].im,
                    };
                    left_rear[inverse_freq_ctr] = Complex {
                        re: left_rear[freq_ctr].re,
                        im: -1.0 * left_rear[freq_ctr].im,
                    };
                    right_rear[inverse_freq_ctr] = Complex {
                        re: right_rear[freq_ctr].re,
                        im: -1.0 * right_rear[freq_ctr].im,
                    };
                }
            }

            self.fft_inverse
                .process_with_scratch(&mut left_front, &mut thread_state.scratch_inverse);
            self.fft_inverse
                .process_with_scratch(&mut right_front, &mut thread_state.scratch_inverse);
            self.fft_inverse
                .process_with_scratch(&mut left_rear, &mut thread_state.scratch_inverse);
            self.fft_inverse
                .process_with_scratch(&mut right_rear, &mut thread_state.scratch_inverse);

            center = match center {
                Some(mut center) => {
                    self.fft_inverse
                        .process_with_scratch(&mut center, &mut thread_state.scratch_inverse);

                    Some(center)
                }
                None => None,
            };

            // Filter LFE
            let lfe = match lfe {
                Some(mut lfe) => {
                    let lfe_levels = self.lfe_levels.as_ref().expect("lfe_levels not set");

                    for window_ctr in 1..thread_state.upmixer.window_midpoint {
                        let (amplitude, phase) = lfe[window_ctr].to_polar();
                        let c = Complex::from_polar(amplitude * lfe_levels[window_ctr], phase);

                        lfe[window_ctr] = c;
                        lfe[thread_state.upmixer.window_size - window_ctr] = Complex {
                            re: c.re,
                            im: -1.0 * c.im,
                        }
                    }

                    self.fft_inverse
                        .process_with_scratch(&mut lfe, &mut thread_state.scratch_inverse);

                    Some(lfe)
                }
                None => None,
            };

            let sample_ctr =
                transformed_window_and_pans.last_sample_ctr - thread_state.upmixer.window_midpoint;

            if sample_ctr == thread_state.upmixer.window_midpoint {
                // Special case for the beginning of the file
                for sample_ctr in 0..sample_ctr {
                    self.write_samples_in_window(
                        &thread_state.upmixer,
                        sample_ctr,
                        sample_ctr as usize,
                        &left_front,
                        &right_front,
                        &left_rear,
                        &right_rear,
                        &lfe,
                        &center,
                    )?;
                }
            } else if transformed_window_and_pans.last_sample_ctr
                == thread_state.upmixer.total_samples_to_write - 1
            {
                // Special case for the end of the file
                let first_sample_in_transform = thread_state.upmixer.total_samples_to_write
                    - thread_state.upmixer.window_size
                    - 1;
                for sample_in_transform in
                    (thread_state.upmixer.window_midpoint - 2)..thread_state.upmixer.window_size
                {
                    self.write_samples_in_window(
                        &thread_state.upmixer,
                        first_sample_in_transform + sample_in_transform,
                        sample_in_transform as usize,
                        &left_front,
                        &right_front,
                        &left_rear,
                        &right_rear,
                        &lfe,
                        &center,
                    )?;
                }
            } else {
                self.write_samples_in_window(
                    &thread_state.upmixer,
                    sample_ctr,
                    thread_state.upmixer.window_midpoint,
                    &left_front,
                    &right_front,
                    &left_rear,
                    &right_rear,
                    &lfe,
                    &center,
                )?;
            }

            thread_state.upmixer.logger.log_status(thread_state)?;
        }

        Ok(())
    }

    fn write_samples_in_window(
        self: &PannerAndWriter,
        upmixer: &Upmixer,
        sample_ctr: usize,
        sample_in_transform: usize,
        left_front: &Vec<Complex<f64>>,
        right_front: &Vec<Complex<f64>>,
        left_rear: &Vec<Complex<f64>>,
        right_rear: &Vec<Complex<f64>>,
        lfe: &Option<Vec<Complex<f64>>>,
        center: &Option<Vec<Complex<f64>>>,
    ) -> Result<()> {
        let gain = db_to_amplitude(0f32 - upmixer.options.headroom.unwrap_or(0.0));

        let mut writer_state = self
            .writer_state
            .lock()
            .expect("Cannot aquire lock because a thread panicked");

        let left_front_sample = left_front[sample_in_transform].re;
        let right_front_sample = right_front[sample_in_transform].re;
        let left_rear_sample = left_rear[sample_in_transform].re;
        let right_rear_sample = right_rear[sample_in_transform].re;

        let lfe_sample = match lfe {
            Some(lfe) => Some(lfe[sample_in_transform].re),
            None => None,
        };

        let center_sample = match center {
            Some(center) => Some(center[sample_in_transform].re),
            None => None,
        };

        let mut samples_by_channel = SamplesByChannel::new()
            .front_left(upmixer.scale * left_front_sample * gain as f64)
            .front_right(upmixer.scale * right_front_sample * gain as f64)
            .back_left(upmixer.scale * left_rear_sample * gain as f64)
            .back_right(upmixer.scale * right_rear_sample * gain as f64);

        match lfe_sample {
            Some(lfe_sample) => {
                samples_by_channel = samples_by_channel.low_frequency(upmixer.scale * lfe_sample);
            }
            None => {}
        }

        match center_sample {
            Some(center_sample) => {
                samples_by_channel = samples_by_channel.front_center(upmixer.scale * center_sample);
            }
            None => {}
        }

        let out_file_index = sample_ctr / self.max_samples_in_file;
        let sample_ctr_in_file = sample_ctr - (self.max_samples_in_file * out_file_index);

        writer_state.target_random_access_wav_writers[out_file_index]
            .write_samples(sample_ctr_in_file, f64_to_f32(samples_by_channel))?;

        writer_state.total_samples_written += 1;

        Ok(())
    }
}

pub fn f64_to_f32(samples: SamplesByChannel<f64>) -> SamplesByChannel<f32> {
    SamplesByChannel {
        front_left_of_center: None,
        front_right_of_center: None,
        back_center: None,
        side_left: None,
        side_right: None,
        top_center: None,
        top_front_left: None,
        top_front_center: None,
        top_front_right: None,
        top_back_left: None,
        top_back_center: None,
        top_back_right: None,
        front_left: samples.front_left.map(|x| x as f32),
        front_right: samples.front_right.map(|x| x as f32),
        front_center: samples.front_center.map(|x| x as f32),
        back_left: samples.back_left.map(|x| x as f32),
        back_right: samples.back_right.map(|x| x as f32),
        low_frequency: samples.low_frequency.map(|x| x as f32),
    }
}

// Perform final flush implicitly
impl Drop for PannerAndWriter {
    fn drop(&mut self) {
        self.writer_state
            .lock()
            .expect("Cannot aquire lock because a thread panicked")
            .target_random_access_wav_writers
            .iter_mut()
            .for_each(|target_random_access_wav_writer| {
                target_random_access_wav_writer
                    .flush()
                    .expect("Can not flush writer")
            });
    }
}
