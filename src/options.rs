use std::env;
use std::path::Path;

use wave_stream::wave_header::Channels;

use crate::{
    matrix::{DefaultMatrix, Matrix, SQMatrix, SQMatrixExperimental},
    panner_and_writer,
};

pub struct Options {
    pub source_wav_path: Box<Path>,
    pub target_wav_path: Box<Path>,
    pub num_threads: Option<usize>,
    pub transform_mono: bool,
    pub channels: Channels,
    pub low_frequency: f32,
    pub minimum_steered_amplitude: f32,
    pub keep_awake: bool,
    pub loud: bool,
    pub requested_fft_size: Option<usize>,
    pub headroom: Option<f32>,

    // Performs additional adjustments according to the specific chosen matrix
    // SQ, QS, RM, ect
    pub matrix: Box<dyn Matrix>,
}

pub enum ChannelLayout {
    Four,
    Five,
    FiveOne,
}

pub enum MatrixFormat {
    Default,
    QS,
    HorseShoe,
    DolbyStereo,
    SQ,
    SQExperimental,
}

impl Options {
    pub fn parse() -> Option<Options> {
        let args: Vec<String> = env::args().collect();

        if args.len() < 3 {
            println!("Usage: soft_matrix [source] [destination]");
            return None;
        }

        let mut args_iter = args.into_iter();

        // ignore the executable name
        let _ = args_iter.next().unwrap();

        let source_wav_path = args_iter.next().unwrap();
        let source_wav_path = Path::new(source_wav_path.as_str());

        let target_wav_path = args_iter.next().unwrap();
        let target_wav_path = Path::new(target_wav_path.as_str());

        let mut num_threads = None;

        let mut channel_layout = ChannelLayout::FiveOne;
        let mut matrix_format = MatrixFormat::Default;
        let mut low_frequency = 20.0f32;

        let mut minimum_steered_amplitude = 0.01;

        let mut keep_awake = true;

        let mut loud: Option<bool> = None;

        let mut fft_size: Option<usize> = None;

        let mut headroom = Some(-24f32);

        // Iterate through the options
        // -channels
        // 4 or 5 or 5.1

        loop {
            match args_iter.next() {
                Some(flag) => {
                    // Parse a flag
                    if flag.eq("-fft_size") {
                        match args_iter.next() {
                            Some(f_size_string) => match f_size_string.parse::<usize>() {
                                Ok(size) => {
                                    if size < 6 {
                                        println!("fft size must >= 6: {}", size);
                                        return None;
                                    }

                                    fft_size = Some(size)
                                }
                                Err(_) => {
                                    println!("fft size must be an integer: {}", f_size_string);
                                    return None;
                                }
                            },
                            None => {
                                println!("fft size unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-headroom") {
                        match args_iter.next() {
                            Some(f_size_string) => match f_size_string.parse::<f32>() {
                                Ok(head) => {
                                    println!("headroom: {}", head);
                                    if head < 0f32 {
                                        println!("headroom must >= 0: {}", head);
                                        return None;
                                    }

                                    headroom = Some(0f32 - head)
                                }
                                Err(_) => {
                                    println!("headroom must be an number: {}", f_size_string);
                                    return None;
                                }
                            },
                            None => {
                                println!("fft size unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-channels") {
                        match args_iter.next() {
                            Some(channels_string) => {
                                if channels_string.eq("4") {
                                    channel_layout = ChannelLayout::Four
                                } else if channels_string.eq("5") {
                                    channel_layout = ChannelLayout::Five
                                } else if channels_string.eq("5.1") {
                                    channel_layout = ChannelLayout::FiveOne
                                } else {
                                    println!("Unknown channel configuration: {}", channels_string);
                                    return None;
                                }
                            }
                            None => {
                                println!("Channels unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-matrix") {
                        match args_iter.next() {
                            Some(matrix_format_string) => {
                                if matrix_format_string.eq("default") {
                                    matrix_format = MatrixFormat::Default
                                } else if matrix_format_string.eq("qs") {
                                    matrix_format = MatrixFormat::QS
                                } else if matrix_format_string.eq("rm") {
                                    matrix_format = MatrixFormat::QS
                                } else if matrix_format_string.eq("horseshoe") {
                                    matrix_format = MatrixFormat::HorseShoe
                                } else if matrix_format_string.eq("dolby") {
                                    matrix_format = MatrixFormat::DolbyStereo
                                } else if matrix_format_string.eq("sq") {
                                    matrix_format = MatrixFormat::SQ
                                } else if matrix_format_string.eq("sqexperimental") {
                                    matrix_format = MatrixFormat::SQExperimental
                                } else {
                                    println!("Unknown matrix format: {}", matrix_format_string);
                                    return None;
                                }
                            }
                            None => {
                                println!("Matrix unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-low") {
                        match args_iter.next() {
                            Some(low_frequency_string) => {
                                match low_frequency_string.parse::<f32>() {
                                    Ok(low_frequency_arg) => {
                                        if low_frequency_arg < 1.0 {
                                            println!(
                                                "Lowest frequency must >= 1: {}",
                                                low_frequency_arg
                                            );
                                            return None;
                                        }

                                        low_frequency = low_frequency_arg
                                    }
                                    Err(_) => {
                                        println!(
                                            "Lowest frequency must be an integer: {}",
                                            low_frequency_string
                                        );
                                        return None;
                                    }
                                }
                            }
                            None => {
                                println!("Lowest frequency unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-threads") {
                        match args_iter.next() {
                            Some(num_threads_string) => match num_threads_string.parse::<usize>() {
                                Ok(num_threads_value) => num_threads = Some(num_threads_value),
                                Err(_) => {
                                    println!(
                                        "Can not parse the number of threads: {}",
                                        num_threads_string
                                    );
                                    return None;
                                }
                            },
                            None => {
                                println!("Number of threads unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-minimum") {
                        match args_iter.next() {
                            Some(minimum_steered_amplitude_string) => {
                                match minimum_steered_amplitude_string.parse::<f32>() {
                                    Ok(minimum_steered_amplitude_value) => {
                                        minimum_steered_amplitude = minimum_steered_amplitude_value
                                    }
                                    Err(_) => {
                                        println!(
                                            "Can not parse the minimum amplitude: {}",
                                            minimum_steered_amplitude_string
                                        );
                                        return None;
                                    }
                                }
                            }
                            None => {
                                println!("Minimum amplitude unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-keepawake") {
                        match args_iter.next() {
                            Some(keep_awake_string) => match keep_awake_string.parse::<bool>() {
                                Ok(keep_awake_value) => keep_awake = keep_awake_value,
                                Err(_) => {
                                    println!(
                                        "Can not parse the keep awake value: {}",
                                        keep_awake_string
                                    );
                                    return None;
                                }
                            },
                            None => {
                                println!("Keepawake value unspecified");
                                return None;
                            }
                        }
                    } else if flag.eq("-loud") {
                        loud = Some(true);
                    } else if flag.eq("-quiet") {
                        loud = Some(false);
                    } else {
                        println!("Unknown flag: {}", flag);
                        return None;
                    }
                }
                None => {
                    // No more flags left, interpret the options and return them
                    let transform_mono: bool;
                    let channels: Channels;

                    match channel_layout {
                        ChannelLayout::Four => {
                            transform_mono = false;
                            channels = Channels::new()
                                .front_left()
                                .front_right()
                                .back_left()
                                .back_right();
                        }
                        ChannelLayout::Five => {
                            transform_mono = true;
                            channels = Channels::new()
                                .front_left()
                                .front_right()
                                .front_center()
                                .back_left()
                                .back_right();
                        }
                        ChannelLayout::FiveOne => {
                            transform_mono = true;
                            channels = Channels::new()
                                .front_left()
                                .front_right()
                                .front_center()
                                .low_frequency()
                                .back_left()
                                .back_right();
                        }
                    }

                    let matrix: Box<dyn Matrix> = match matrix_format {
                        MatrixFormat::Default => Box::new(DefaultMatrix::new()),
                        MatrixFormat::QS => Box::new(DefaultMatrix::qs()),
                        MatrixFormat::HorseShoe => Box::new(DefaultMatrix::horseshoe()),
                        MatrixFormat::DolbyStereo => Box::new(DefaultMatrix::dolby_stereo()),
                        MatrixFormat::SQ => Box::new(SQMatrix::sq()),
                        MatrixFormat::SQExperimental => Box::new(SQMatrixExperimental::sq()),
                    };

                    if (low_frequency as f64) > panner_and_writer::LFE_START
                        && channels.low_frequency
                    {
                        println!(
                            "LFE channel not supported when the lowest frequency to steer ({}hz) is greater than {}hz",
                            low_frequency,
                            panner_and_writer::LFE_START);
                        return None;
                    }

                    let loud = if transform_mono {
                        loud.unwrap_or(false)
                    } else {
                        if loud.is_some() {
                            println!("-loud and -quiet only work when upmixing with an LFE or a center channel");
                            return None;
                        }

                        true
                    };

                    return Some(Options {
                        source_wav_path: source_wav_path.into(),
                        target_wav_path: target_wav_path.into(),
                        num_threads,
                        transform_mono,
                        channels,
                        matrix,
                        low_frequency,
                        minimum_steered_amplitude,
                        keep_awake,
                        loud,
                        requested_fft_size: fft_size,
                        headroom,
                    });
                }
            }
        }
    }
}
pub fn amplitude_to_db(amplitude: f32) -> f32 {
    return 20.0 * amplitude.log10();
}

pub fn db_to_amplitude(db: f32) -> f32 {
    return 10.0f32.powf(db / 20.0);
}
