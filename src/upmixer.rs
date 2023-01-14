use std::collections::{HashMap, VecDeque};
use std::f32::consts::{PI, TAU};
use std::io::{stdout, Read, Result, Seek, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::available_parallelism;
use std::time::{Duration, Instant};

use rustfft::Fft;
use rustfft::{num_complex::Complex, FftPlanner};
use wave_stream::open_wav::OpenWav;
use wave_stream::wave_reader::{OpenWavReader, RandomAccessOpenWavReader};
use wave_stream::wave_writer::OpenWavWriter;

use crate::structs::{
    FrequencyPosition, OpenWavReaderAndBuffer, TransformedWindowAndPans,
    /*UpmixedWindow,*/ WriterState,
};
use crate::window_sizes::get_ideal_window_size;

struct Upmixer {
    open_wav_reader_and_buffer: Mutex<OpenWavReaderAndBuffer>,
    window_size: usize,
    midpoint: usize,
    scale: f32,

    // Temporary location for transformed windows and pans so that they can be finished out-of-order
    transformed_window_and_pans_by_sample: Mutex<HashMap<u32, TransformedWindowAndPans>>,

    // A queue of transformed windows and all of the panned locations of each frequency
    transformed_window_and_pans_queue: Mutex<VecDeque<TransformedWindowAndPans>>,

    // A queue of transformed windows and the average pans of each frequency
    transformed_window_and_pans_averages_queue: Mutex<VecDeque<TransformedWindowAndPans>>,

    // All of the upmixed samples, indexed by their sample count
    // Threads can write to this as they finish upmixing a window, even out-of-order
    //upmixed_windows_by_sample: Mutex<HashMap<u32, UpmixedWindow>>,

    // Wav writer and state used to communicate status
    writer_state: Mutex<WriterState>,
}

unsafe impl Send for Upmixer {}
unsafe impl Sync for Upmixer {}

pub fn upmix<TReader: 'static + Read + Seek>(
    source_wav_reader: OpenWavReader<TReader>,
    target_wav_writer: OpenWavWriter,
) -> Result<()> {
    let mut stdout = stdout();
    stdout.write(format!("Starting...").as_bytes())?;
    stdout.flush()?;

    let min_window_size = source_wav_reader.sample_rate() / 10;
    let window_size = get_ideal_window_size(min_window_size as usize)?;

    let source_wav_reader = source_wav_reader.get_random_access_f32_reader()?;
    let target_wav_writer = target_wav_writer.get_random_access_f32_writer()?;

    // rustfft states that the scale is 1/len()
    // See "noramlization": https://docs.rs/rustfft/latest/rustfft/#normalization
    // However, going back and forth to polar coordinates appears to make this very quiet, so I swapped
    // to 2 / ...
    let scale: f32 = 2.0 / (window_size as f32);

    /*
    // The upmixed queue must be padded with empty upmixed windows so that there are seed windows before
    // the first upmixed samples
    let mut upmixed_queue = VecDeque::<UpmixedWindow>::new();
    pad_upmixed_queue(window_size, &mut upmixed_queue);
    */

    let mut open_wav_reader_and_buffer = OpenWavReaderAndBuffer {
        source_wav_reader: source_wav_reader,
        next_read_sample: 0,
        // The read buffers must be padded for the first window. Padding is half a window of silence, and then
        // half a window (minus one sample) of the beginning of the wav
        left_buffer: VecDeque::from(vec![
            Complex {
                re: 0.0f32,
                im: 0.0f32,
            };
            window_size / 2
        ]),
        right_buffer: VecDeque::from(vec![
            Complex {
                re: 0.0f32,
                im: 0.0f32,
            };
            window_size / 2
        ]),
    };

    for sample_to_read in 0..((window_size / 2) - 1) as u32 {
        read_samples(sample_to_read, &mut open_wav_reader_and_buffer)?;
    }

    let now = Instant::now();
    let total_samples_to_write = open_wav_reader_and_buffer
        .source_wav_reader
        .info()
        .len_samples() as f64;

    let upmixer = Arc::new(Upmixer {
        open_wav_reader_and_buffer: Mutex::new(open_wav_reader_and_buffer),
        window_size,
        midpoint: window_size / 2,
        scale,
        transformed_window_and_pans_by_sample: Mutex::new(HashMap::new()),
        transformed_window_and_pans_queue: Mutex::new(VecDeque::new()),
        transformed_window_and_pans_averages_queue: Mutex::new(VecDeque::new()),
        //upmixed_windows_by_sample: Mutex::new(HashMap::new()),
        writer_state: Mutex::new(WriterState {
            //upmixed_queue,
            target_wav_writer,
            started: now,
            next_log: now,
            total_samples_to_write,
        }),
    });

    // Start threads
    let num_threads = available_parallelism()?.get();
    let mut join_handles = Vec::with_capacity(num_threads - 1);
    for _ in 1..num_threads {
        let upmixer_thread = upmixer.clone();
        let join_handle = thread::spawn(move || {
            upmixer_thread.run_upmix_thread();
        });

        join_handles.push(join_handle);
    }

    // Perform upmixing on this thread as well
    upmixer.run_upmix_thread();

    for join_handle in join_handles {
        // Note that threads will terminate the process if there is an unhandled error
        join_handle.join().expect("Could not join thread");
    }

    stdout.write(
        format!("\rFinishing...                                                                 ")
            .as_bytes(),
    )?;
    println!();
    stdout.flush()?;

    {
        let mut writer_state = upmixer
            .writer_state
            .lock()
            .expect("Cannot aquire lock because a thread panicked");

        // TODO: Final padding and final samples
        /*
        pad_upmixed_queue(window_size, &mut queue_and_writer.upmixed_queue);
        upmixer.write_samples(&mut queue_and_writer)?;
        */

        writer_state.target_wav_writer.flush()?;
    }
    Ok(())
}

/*
fn pad_upmixed_queue(window_size: usize, upmixed_queue: &mut VecDeque<UpmixedWindow>) {
    let first_sample = match upmixed_queue.back() {
        Some(upmixed_window) => upmixed_window.sample_ctr + 1,
        None => 0,
    };

    let window_size_u32 = window_size as u32;

    for sample_ctr in first_sample..(first_sample + window_size_u32 / 2) {
        upmixed_queue.push_front(UpmixedWindow {
            sample_ctr,
            left_front: vec![Complex { re: 0f32, im: 0f32 }; window_size],
            right_front: vec![Complex { re: 0f32, im: 0f32 }; window_size],
            left_rear: vec![Complex { re: 0f32, im: 0f32 }; window_size],
            right_rear: vec![Complex { re: 0f32, im: 0f32 }; window_size],
        })
    }
}
*/

fn read_samples(
    sample_to_read: u32,
    open_wav_reader_and_buffer: &mut OpenWavReaderAndBuffer,
) -> Result<()> {
    let len_samples = open_wav_reader_and_buffer
        .source_wav_reader
        .info()
        .len_samples();

    if sample_to_read < len_samples {
        let left = open_wav_reader_and_buffer
            .source_wav_reader
            .read_sample(sample_to_read, 0)?;
        open_wav_reader_and_buffer.left_buffer.push_back(Complex {
            re: left,
            im: 0.0f32,
        });

        let right = open_wav_reader_and_buffer
            .source_wav_reader
            .read_sample(sample_to_read, 1)?;
        open_wav_reader_and_buffer.right_buffer.push_back(Complex {
            re: right,
            im: 0.0f32,
        });
    } else {
        // The read buffer needs to be padded with empty samples, this way there is a full window to
        // run an fft on the end of the wav

        open_wav_reader_and_buffer.left_buffer.push_back(Complex {
            re: 0.0f32,
            im: 0.0f32,
        });
        open_wav_reader_and_buffer.right_buffer.push_back(Complex {
            re: 0.0f32,
            im: 0.0f32,
        });
    }

    Ok(())
}

impl Upmixer {
    // Runs the upmix thread. Aborts the process if there is an error
    fn run_upmix_thread(self: &Upmixer) {
        match self.run_upmix_thread_int() {
            Err(error) => {
                println!("Error upmixing: {:?}", error);
                std::process::exit(-1);
            }
            _ => {}
        }
    }

    fn run_upmix_thread_int(self: &Upmixer) -> Result<()> {
        // Each thread has its own separate FFT calculator
        let mut planner = FftPlanner::new();
        let fft_forward = planner.plan_fft_forward(self.window_size);
        let fft_inverse = planner.plan_fft_inverse(self.window_size);

        // Each thread has separateF FFT scratch space
        let mut scratch_forward = vec![
            Complex {
                re: 0.0f32,
                im: 0.0f32
            };
            fft_forward.get_inplace_scratch_len()
        ];
        let mut scratch_inverse = vec![
            Complex {
                re: 0.0f32,
                im: 0.0f32
            };
            fft_inverse.get_inplace_scratch_len()
        ];

        'upmix_each_sample: loop {
            let transformed_window_and_pans_option =
                self.transform_and_measure_pans(&fft_forward, &mut scratch_forward)?;

            // Break the loop if upmix_sample returned None
            let transformed_window_and_pans = match transformed_window_and_pans_option {
                Some(transformed_window_and_pans) => transformed_window_and_pans,
                None => {
                    break 'upmix_each_sample;
                }
            };

            // Use a separate lock on transformed_window_and_pans_by_sample so that threads not in enqueue_and_average
            // can keep processing
            {
                let mut transformed_window_and_pans_by_sample = self
                    .transformed_window_and_pans_by_sample
                    .lock()
                    .expect("Cannot aquire lock because a thread panicked");

                transformed_window_and_pans_by_sample.insert(
                    transformed_window_and_pans.sample_ctr,
                    transformed_window_and_pans,
                );
            }

            // If a lock can be aquired
            // - Enqueues completed transformed_window_and_pans
            // - Performs averaging
            //
            // The conditional lock is because these calculations require global state and can not be
            // performed in parallel
            self.enqueue_and_average()?;

            /*
            // Use a separate lock on upmixed_windows so that threads not in write_samples_from_upmixed_queue
            // can keep processing
            {
                let mut upmixed_windows_by_sample = self
                    .upmixed_windows_by_sample
                    .lock()
                    .expect("Cannot aquire lock because a thread panicked");

                upmixed_windows_by_sample.insert(transformed_window_and_pans.1.sample_ctr, transformed_window_and_pans.1);
            }

            self.write_samples_from_upmixed_queue()?;
            */

            self.perform_backwards_transform_and_write_samples(&fft_inverse, &mut scratch_inverse)?;
        }

        // It's possible that there are dangling samples on the queue
        // Because write_samples_from_upmixed_queue doesn't wait for the lock, this should be called
        // one more time to drain the queue of upmixed samples
        self.perform_backwards_transform_and_write_samples(&fft_inverse, &mut scratch_inverse)?;

        Ok(())
    }

    fn transform_and_measure_pans(
        self: &Upmixer,
        fft_forward: &Arc<dyn Fft<f32>>,
        scratch_forward: &mut Vec<Complex<f32>>,
    ) -> Result<Option<TransformedWindowAndPans>> {
        let mut left_front: Vec<Complex<f32>>;
        let mut right_front: Vec<Complex<f32>>;
        let sample_ctr: u32;

        {
            let mut open_wav_reader_and_buffer = self
                .open_wav_reader_and_buffer
                .lock()
                .expect("Cannot aquire lock because a thread panicked");

            let source_len = open_wav_reader_and_buffer
                .source_wav_reader
                .info()
                .len_samples() as u32;

            sample_ctr = open_wav_reader_and_buffer.next_read_sample;
            if sample_ctr >= source_len {
                return Ok(None);
            } else {
                open_wav_reader_and_buffer.next_read_sample += 1;
            }

            let sample_to_read = sample_ctr as u32 + ((self.window_size / 2) as u32);
            read_samples(sample_to_read, &mut open_wav_reader_and_buffer)?;

            // Read queues are copied so that there are windows for running FFTs
            // (At one point I had each thread read the entire window from the wav reader. That was much
            // slower and caused lock contention)
            left_front = Vec::from(open_wav_reader_and_buffer.left_buffer.make_contiguous());
            right_front = Vec::from(open_wav_reader_and_buffer.right_buffer.make_contiguous());

            // After the window is read, pop the unneeded samples (for the next read)
            open_wav_reader_and_buffer.left_buffer.pop_front();
            open_wav_reader_and_buffer.right_buffer.pop_front();
        }

        fft_forward.process_with_scratch(&mut left_front, scratch_forward);
        fft_forward.process_with_scratch(&mut right_front, scratch_forward);

        // TODO: After transitioning to TransformedWindowAndPans, these should not be copies
        let left_transformed = left_front.to_vec();
        let right_transformed = right_front.to_vec();
        let mut frequency_positions = Vec::with_capacity(self.midpoint);

        // Rear channels start as copies of the front channels
        let mut left_rear = left_front.to_vec();
        let mut right_rear = right_front.to_vec();

        // Ultra-lows are not shitfted
        left_rear[0] = Complex { re: 0f32, im: 0f32 };
        right_rear[0] = Complex { re: 0f32, im: 0f32 };

        for freq_ctr in 1..(self.midpoint + 1) {
            // Phase is offset from sine/cos in # of samples
            let left = left_front[freq_ctr];
            let (_left_amplitude, left_phase) = left.to_polar();
            let right = right_front[freq_ctr];
            let (_right_amplitude, right_phase) = right.to_polar();

            // Will range from 0 to tau
            // 0 is in phase, pi is out of phase, tau is in phase (think circle)
            let phase_difference_tau = (left_phase - right_phase).abs();

            // 0 is in phase, pi is out of phase, tau is in phase (think half circle)
            let phase_difference_pi = if phase_difference_tau > PI {
                PI - (TAU - phase_difference_tau)
            } else {
                phase_difference_tau
            };

            // phase ratio: 0 is in phase, 1 is out of phase
            let back_to_front = phase_difference_pi / PI;

            frequency_positions.push(FrequencyPosition { back_to_front });
        }

        let transformed_window_and_pans = TransformedWindowAndPans {
            sample_ctr,
            left_transformed,
            right_transformed,
            frequency_positions,
        };

        return Ok(Some(transformed_window_and_pans));
    }

    fn enqueue_and_average(self: &Upmixer) -> Result<()> {
        // The thread that can lock self.transformed_window_and_pans_queue will keep writing samples are long as there
        // are samples to write
        // All other threads will skip this logic and continue performing FFTs while a thread has this lock
        let mut transformed_window_and_pans_queue =
            match self.transformed_window_and_pans_queue.try_lock() {
                Ok(queue_and_writer) => queue_and_writer,
                // Some other thread is writing samples from the sample queue
                Err(_) => {
                    return Ok(());
                }
            };

        {
            // Get locks and the last_sample_ctr to...
            let mut next_sample_ctr = match transformed_window_and_pans_queue.back() {
                Some(last_window) => last_window.sample_ctr + 1,
                None => 0,
            };

            let mut transformed_window_and_pans_by_sample = self
                .transformed_window_and_pans_by_sample
                .lock()
                .expect("Cannot aquire lock because a thread panicked");

            // ...fill the queue with all finished upmixed samples
            'enqueue: loop {
                match transformed_window_and_pans_by_sample.remove(&next_sample_ctr) {
                    Some(upmixed_window) => {
                        transformed_window_and_pans_queue.push_back(upmixed_window);
                    }
                    None => break 'enqueue,
                }

                next_sample_ctr += 1;
            }
        }

        // While transformed_window_and_pans_queue is large enough, average the pans
        // and enqueue the averages
        let midpoint = self.window_size / 2;
        let window_size_f32 = self.window_size as f32;
        while transformed_window_and_pans_queue.len() >= self.window_size {
            let mut frequency_position_averages =
                vec![FrequencyPosition { back_to_front: 0.0 }; midpoint];

            // Sum all the pans within the window...
            for sample_ctr in 0..self.window_size {
                for freq_ctr in 0..midpoint {
                    let front_to_back_itr = transformed_window_and_pans_queue[sample_ctr]
                        .frequency_positions[freq_ctr]
                        .back_to_front;
                    frequency_position_averages[freq_ctr].back_to_front += front_to_back_itr;
                }
            }

            // ... To get the average pans in the window
            for freq_ctr in 0..midpoint {
                frequency_position_averages[freq_ctr].back_to_front /= window_size_f32;
            }

            // Construct another TransformedWindowAndPans with the averages...
            let midpoint_transformed_window_and_pans = &transformed_window_and_pans_queue[midpoint];
            let transformed_window_and_pans_averages = TransformedWindowAndPans {
                sample_ctr: midpoint_transformed_window_and_pans.sample_ctr,
                // TODO: Try exchanging this with a 0-length vector to avoid the copy
                left_transformed: midpoint_transformed_window_and_pans
                    .left_transformed
                    .to_vec(),
                right_transformed: midpoint_transformed_window_and_pans
                    .right_transformed
                    .to_vec(),
                frequency_positions: frequency_position_averages,
            };

            // ... And enqueue it into the queue of averages
            let mut transformed_window_and_pans_averages_queue = self
                .transformed_window_and_pans_averages_queue
                .lock()
                .expect("Cannot aquire lock because a thread panicked");

            transformed_window_and_pans_averages_queue
                .push_back(transformed_window_and_pans_averages);

            // Remove the unneeded transform
            transformed_window_and_pans_queue.pop_front();
        }

        Ok(())
    }

    fn perform_backwards_transform_and_write_samples(
        self: &Upmixer,
        fft_inverse: &Arc<dyn Fft<f32>>,
        scratch_inverse: &mut Vec<Complex<f32>>,
    ) -> Result<()> {
        'transform_and_write: loop {
            let transformed_window_and_pans = {
                let mut transformed_window_and_pans_averages_queue = self
                    .transformed_window_and_pans_averages_queue
                    .lock()
                    .expect("Cannot aquire lock because a thread panicked");

                match transformed_window_and_pans_averages_queue.pop_front() {
                    Some(transformed_window_and_pans) => transformed_window_and_pans,
                    None => {
                        break 'transform_and_write;
                    }
                }
            };

            let mut left_front = transformed_window_and_pans.left_transformed;
            let mut right_front = transformed_window_and_pans.right_transformed;

            // Rear channels start as copies of the front channels
            let mut left_rear = left_front.to_vec();
            let mut right_rear = right_front.to_vec();

            // Ultra-lows are not shitfted
            left_rear[0] = Complex { re: 0f32, im: 0f32 };
            right_rear[0] = Complex { re: 0f32, im: 0f32 };

            for freq_ctr in 1..(self.midpoint + 1) {
                // Phase is offset from sine/cos in # of samples
                let left = left_front[freq_ctr];
                let (left_amplitude, left_phase) = left.to_polar();
                let right = right_front[freq_ctr];
                let (right_amplitude, right_phase) = right.to_polar();

                // phase ratio: 0 is in phase, 1 is out of phase
                let frequency_position =
                    &transformed_window_and_pans.frequency_positions[freq_ctr - 1];
                let back_to_front = frequency_position.back_to_front;
                let front_to_back = 1f32 - back_to_front;

                // Figure out the amplitudes for front and rear
                let left_front_amplitude = left_amplitude * front_to_back;
                let right_front_amplitude = right_amplitude * front_to_back;
                let left_rear_amplitude = left_amplitude * back_to_front;
                let right_rear_amplitude = right_amplitude * back_to_front;

                // Assign to array
                left_front[freq_ctr] = Complex::from_polar(left_front_amplitude, left_phase);
                right_front[freq_ctr] = Complex::from_polar(right_front_amplitude, right_phase);
                left_rear[freq_ctr] = Complex::from_polar(left_rear_amplitude, left_phase);
                right_rear[freq_ctr] = Complex::from_polar(right_rear_amplitude, right_phase);

                if freq_ctr < self.midpoint {
                    let inverse_freq_ctr = self.window_size - freq_ctr;
                    left_front[inverse_freq_ctr] = left_front[freq_ctr];
                    right_front[inverse_freq_ctr] = right_front[freq_ctr];
                    left_rear[inverse_freq_ctr] = left_rear[freq_ctr];
                    right_rear[inverse_freq_ctr] = right_rear[freq_ctr];
                }
            }

            fft_inverse.process_with_scratch(&mut left_front, scratch_inverse);
            fft_inverse.process_with_scratch(&mut right_front, scratch_inverse);
            fft_inverse.process_with_scratch(&mut left_rear, scratch_inverse);
            fft_inverse.process_with_scratch(&mut right_rear, scratch_inverse);

            {
                let mut writer_state = self
                    .writer_state
                    .lock()
                    .expect("Cannot aquire lock because a thread panicked");

                let sample_ctr = transformed_window_and_pans.sample_ctr;
                let left_front_sample = left_front[self.midpoint].re;
                let right_front_sample = right_front[self.midpoint].re;
                let left_rear_sample = left_rear[self.midpoint].re;
                let right_rear_sample = right_rear[self.midpoint].re;

                writer_state.target_wav_writer.write_sample(
                    sample_ctr,
                    0,
                    self.scale * left_front_sample,
                )?;
                writer_state.target_wav_writer.write_sample(
                    sample_ctr,
                    1,
                    self.scale * right_front_sample,
                )?;
                writer_state.target_wav_writer.write_sample(
                    sample_ctr,
                    2,
                    self.scale * left_rear_sample,
                )?;
                writer_state.target_wav_writer.write_sample(
                    sample_ctr,
                    3,
                    self.scale * right_rear_sample,
                )?;

                // Log current progess
                let now = Instant::now();
                if now >= writer_state.next_log {
                    let elapsed_seconds = (now - writer_state.started).as_secs_f64();
                    let fraction_complete =
                        (sample_ctr as f64) / writer_state.total_samples_to_write;
                    let estimated_seconds = elapsed_seconds / fraction_complete;

                    let mut stdout = stdout();
                    stdout.write(
                        format!(
                            "\rWriting: {:.2}% complete, {:.0} elapsed seconds, {:.2} estimated total seconds",
                            100.0 * fraction_complete,
                            elapsed_seconds,
                            estimated_seconds,
                        )
                        .as_bytes(),
                    )?;
                    stdout.flush()?;

                    writer_state.next_log += Duration::from_secs(1);
                }
            }
        }

        Ok(())
    }

    /*
    fn write_samples_from_upmixed_queue(self: &Upmixer) -> Result<()> {
        // The thread that can lock self.queue_and_writer will keep writing samples are long as there
        // are samples to write
        // All other threads will skip this logic and continue performing FFTs while a thread has this lock
        let mut queue_and_writer = match self.writer_state.try_lock() {
            Ok(queue_and_writer) => queue_and_writer,
            // Some other thread is writing samples from the sample queue
            Err(_) => {
                return Ok(());
            }
        };

        let last_sample_processed = {
            // Get locks and the last_sample_ctr to...
            let mut upmixed_windows_by_sample = self
                .upmixed_windows_by_sample
                .lock()
                .expect("Cannot aquire lock because a thread panicked");

            let mut last_sample_ctr = match queue_and_writer.upmixed_queue.back() {
                Some(last_window) => last_window.sample_ctr,
                None => 0,
            };

            // ...fill the queue with all finished upmixed samples
            'enqueue: loop {
                match upmixed_windows_by_sample.remove(&(last_sample_ctr + 1)) {
                    Some(upmixed_window) => {
                        queue_and_writer.upmixed_queue.push_back(upmixed_window);
                    }
                    None => break 'enqueue,
                }

                last_sample_ctr += 1;
            }

            last_sample_ctr as f64
        };

        // Release the lock on upmixed_windows_by_sample so other threads can write into it
        // Keep queue_and_writer locked so that the samples can be written
        self.write_samples(queue_and_writer.deref_mut())?;

        // Log current progess
        let now = Instant::now();
        if now >= queue_and_writer.next_log {
            let elapsed_seconds = (now - queue_and_writer.started).as_secs_f64();
            let fraction_complete = last_sample_processed / queue_and_writer.total_samples_to_write;
            let estimated_seconds = elapsed_seconds / fraction_complete;

            let mut stdout = stdout();
            stdout.write(
                format!(
                    "\rWriting: {:.2}% complete, {:.0} elapsed seconds, {:.2} estimated total seconds",
                    100.0 * fraction_complete,
                    elapsed_seconds,
                    estimated_seconds,
                )
                .as_bytes(),
            )?;
            stdout.flush()?;

            queue_and_writer.next_log += Duration::from_secs(1);
        }

        Ok(())
    }

    fn write_samples(self: &Upmixer, queue_and_writer: &mut WriterState) -> Result<()> {
        // Write all upmixed samples until the queue is smaller than the window size
        while queue_and_writer.upmixed_queue.len() >= self.window_size {
            let mut left_front_sample = 0f32;
            let mut right_front_sample = 0f32;
            let mut left_rear_sample = 0f32;
            let mut right_rear_sample = 0f32;

            // Each sample to write is...
            for queue_ctr in 0..self.window_size {
                let upmixed_window = &queue_and_writer.upmixed_queue[queue_ctr];
                left_front_sample += upmixed_window.left_front[queue_ctr].re;
                right_front_sample += upmixed_window.right_front[queue_ctr].re;
                left_rear_sample += upmixed_window.left_rear[queue_ctr].re;
                right_rear_sample += upmixed_window.right_rear[queue_ctr].re;
            }

            let sample_ctr_to_write =
                queue_and_writer.upmixed_queue[self.window_size / 2].sample_ctr as u32;

            // The average of that sample in each upmixed window
            let window_size_f32 = self.window_size as f32;
            left_front_sample /= window_size_f32;
            right_front_sample /= window_size_f32;
            left_rear_sample /= window_size_f32;
            right_rear_sample /= window_size_f32;

            queue_and_writer.target_wav_writer.write_sample(
                sample_ctr_to_write,
                0,
                self.scale * left_front_sample,
            )?;
            queue_and_writer.target_wav_writer.write_sample(
                sample_ctr_to_write,
                1,
                self.scale * right_front_sample,
            )?;
            queue_and_writer.target_wav_writer.write_sample(
                sample_ctr_to_write,
                2,
                self.scale * left_rear_sample,
            )?;
            queue_and_writer.target_wav_writer.write_sample(
                sample_ctr_to_write,
                3,
                self.scale * right_rear_sample,
            )?;

            queue_and_writer.upmixed_queue.pop_front();
        }

        Ok(())
    }*/
}
