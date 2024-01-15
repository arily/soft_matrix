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
    pub channel_layout: ChannelLayout,
    pub transform_mono: bool,
    pub channels: Channels,
    pub low_frequency: f32,
    pub minimum_steered_amplitude: f32,
    pub keep_awake: bool,

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

        // Iterate through the options
        // -channels
        // 4 or 5 or 5.1

        loop {
            match args_iter.next() {
                Some(flag) => {
                    // Parse a flag
                    if flag.eq("-channels") {
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

                    if (low_frequency as f32) > panner_and_writer::LFE_START
                        && channels.low_frequency
                    {
                        println!(
                            "LFE channel not supported when the lowest frequency to steer ({}hz) is greater than {}hz",
                            low_frequency,
                            panner_and_writer::LFE_START);
                        return None;
                    }

                    return Some(Options {
                        source_wav_path: source_wav_path.into(),
                        target_wav_path: target_wav_path.into(),
                        num_threads,
                        channel_layout,
                        transform_mono,
                        channels,
                        matrix,
                        low_frequency,
                        minimum_steered_amplitude,
                        keep_awake,
                    });
                }
            }
        }
    }
}
