# Rust Mobile Music Player v1.1

A touch-first Mobile Music Player written in modern Rust using:
- **GUI Engine:** `egui` & `eframe` 0.30 with `android-activity`
- **Audio Decoding & DSP:** `rodio` 0.20 with Symphonia (MP3, WAV, FLAC, OGG, M4A) + 5-Band Biquad EQ
- **Tag Extraction:** `lofty` 0.21 (ID3v2 tags, embedded album art, duration)
- **Concurrency:** `crossbeam-channel` 0.5 (lock-free UI <-> Audio thread)
- **APK Target:** `aarch64-linux-android` (64-bit ARM Android devices)

---

## Automated Cloud APK via GitHub Actions
Push this project to GitHub and open **Actions** → **Build Android APK** → **Run workflow** to produce a signed APK using `cargo-ndk` and Gradle 8.7.

---

## Local Desktop & Android NDK Build

```bash
# Run smartphone simulator on desktop
cargo run

# Or build Android shared library (.so) and APK
rustup target add aarch64-linux-android
cargo install cargo-ndk
cargo ndk -t aarch64-linux-android -o ./android/app/src/main/jniLibs build --release
cd android && gradle assembleRelease
```
