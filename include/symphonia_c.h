#ifndef SYMPHONIA_C_H
#define SYMPHONIA_C_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int32_t symphonia_c_status_t;

enum {
    SYMPHONIA_C_OK = 0,
    SYMPHONIA_C_END_OF_STREAM = 1,
    SYMPHONIA_C_INVALID_ARGUMENT = -1,
    SYMPHONIA_C_IO_ERROR = -2,
    SYMPHONIA_C_DECODE_ERROR = -3,
    SYMPHONIA_C_UNSUPPORTED = -4,
    SYMPHONIA_C_LIMIT_ERROR = -5,
    SYMPHONIA_C_INTERNAL_ERROR = -6,
};

typedef struct SymphoniaCDecoder symphonia_c_decoder_t;

typedef struct SymphoniaCAudioBuffer {
    float *samples;
    size_t sample_count;
    size_t frames;
    uint32_t channels;
    uint32_t sample_rate;
} SymphoniaCAudioBuffer;

symphonia_c_status_t symphonia_c_open_file(const char *path, symphonia_c_decoder_t **out_decoder);
void symphonia_c_free(symphonia_c_decoder_t *decoder);

uint32_t symphonia_c_sample_rate(const symphonia_c_decoder_t *decoder);
uint32_t symphonia_c_channels(const symphonia_c_decoder_t *decoder);
uint64_t symphonia_c_total_frames(const symphonia_c_decoder_t *decoder);

/*
 * Seeks to a frame offset from the start of the selected audio track.
 * The next symphonia_c_read_f32 call starts at the requested frame when the
 * underlying format can seek to or before it. out_actual_frame may be NULL.
 */
symphonia_c_status_t symphonia_c_seek_frame(
    symphonia_c_decoder_t *decoder,
    uint64_t frame,
    uint64_t *out_actual_frame);

/*
 * Seeks to a time offset in seconds from the start of the selected audio track.
 * out_actual_seconds may be NULL.
 */
symphonia_c_status_t symphonia_c_seek_seconds(
    symphonia_c_decoder_t *decoder,
    double seconds,
    double *out_actual_seconds);

/*
 * Reads interleaved float32 PCM into out_samples.
 * out_samples must have room for frame_capacity * symphonia_c_channels(decoder) floats.
 * Returns SYMPHONIA_C_END_OF_STREAM when no more frames are available.
 */
symphonia_c_status_t symphonia_c_read_f32(
    symphonia_c_decoder_t *decoder,
    float *out_samples,
    size_t frame_capacity,
    size_t *out_frames);

/*
 * Convenience helper that decodes the whole file into one Rust-owned buffer.
 * Release buffer.samples with symphonia_c_audio_buffer_free.
 */
symphonia_c_status_t symphonia_c_decode_file_f32(
    const char *path,
    SymphoniaCAudioBuffer *out_buffer);
void symphonia_c_audio_buffer_free(SymphoniaCAudioBuffer *buffer);

const char *symphonia_c_last_error(const symphonia_c_decoder_t *decoder);
const char *symphonia_c_status_message(symphonia_c_status_t status);

#ifdef __cplusplus
}
#endif

#endif
