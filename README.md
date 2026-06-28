# symphonia-c

C ABI wrapper around Symphonia for decoding audio files to interleaved `float`
PCM.

## Supported formats

The enabled Symphonia features currently cover FLAC, MP3/MPA, WAV/PCM, and
OGG/Vorbis.

M4A/AAC is not enabled because the `symphonia 0.6.0` crate currently cannot
resolve the matching `symphonia-codec-aac`, `symphonia-codec-alac`, and
`symphonia-format-isomp4` `0.6.0` crates from crates.io in this environment.

## Build

```sh
cargo build --release
```

The build produces:

- `target/release/libsymphonia_c.a`
- `target/release/libsymphonia_c.dylib`
- `include/symphonia_c.h`

## iOS XCFramework

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim

cargo build --release --target aarch64-apple-ios
cargo build --release --target aarch64-apple-ios-sim

xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libsymphonia_c.a \
  -headers include \
  -library target/aarch64-apple-ios-sim/release/libsymphonia_c.a \
  -headers include \
  -output SymphoniaC.xcframework
```

For Intel simulator support, also build `x86_64-apple-ios` and combine the
simulator static libraries with `lipo` before creating the XCFramework.

## C usage

```c
#include "symphonia_c.h"

SymphoniaCAudioBuffer buffer = {0};
symphonia_c_status_t status = symphonia_c_decode_file_f32(path, &buffer);

if (status == SYMPHONIA_C_OK) {
    // buffer.samples is interleaved float PCM.
    // buffer.sample_count == buffer.frames * buffer.channels.
}
else {
    const char *message = symphonia_c_last_error(NULL);
}

symphonia_c_audio_buffer_free(&buffer);
```

For streaming, use `symphonia_c_open_file`, query `symphonia_c_channels` and
`symphonia_c_sample_rate`, repeatedly call `symphonia_c_read_f32`, then release
the handle with `symphonia_c_free`.
