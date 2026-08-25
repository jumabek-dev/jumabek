use crate::voice::vad::SAMPLE_RATE;

const HEADER_BYTES: u32 = 36;
const BITS_PER_SAMPLE: u16 = 16;
const CHANNELS: u16 = 1;

pub fn pcm_to_wav(samples: &[i16]) -> Vec<u8> {
    let bytes_per_sample = (BITS_PER_SAMPLE / 8) as u32;
    let data_size = samples.len() as u32 * bytes_per_sample;
    let byte_rate = SAMPLE_RATE * CHANNELS as u32 * bytes_per_sample;
    let block_align = CHANNELS * (BITS_PER_SAMPLE / 8);

    let mut wav = Vec::with_capacity(44 + data_size as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(HEADER_BYTES + data_size).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_size.to_le_bytes());

    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_riff_header() {
        let wav = pcm_to_wav(&[0, 1, -1]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[36..40], b"data");
    }

    #[test]
    fn sizes_add_up() {
        let samples = vec![0i16; 1000];
        let wav = pcm_to_wav(&samples);

        assert_eq!(wav.len(), 44 + samples.len() * 2);

        let riff_size = u32::from_le_bytes(wav[4..8].try_into().unwrap());
        assert_eq!(riff_size as usize, wav.len() - 8);

        let data_size = u32::from_le_bytes(wav[40..44].try_into().unwrap());
        assert_eq!(data_size as usize, samples.len() * 2);
    }

    #[test]
    fn declares_mono_16bit_at_the_capture_rate() {
        let wav = pcm_to_wav(&[0; 10]);
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1);
        assert_eq!(
            u32::from_le_bytes(wav[24..28].try_into().unwrap()),
            SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
    }

    #[test]
    fn samples_survive_the_round_trip() {
        let samples: Vec<i16> = vec![0, 1234, -1234, i16::MAX, i16::MIN];
        let wav = pcm_to_wav(&samples);

        let decoded: Vec<i16> = wav[44..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();

        assert_eq!(decoded, samples);
    }

    #[test]
    fn empty_input_still_makes_a_valid_file() {
        let wav = pcm_to_wav(&[]);
        assert_eq!(wav.len(), 44);
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 0);
    }
}
