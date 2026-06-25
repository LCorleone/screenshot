# screenshot-dai

A lightweight Windows screenshot tool with OCR, LLM chat, and translation — built in Rust.

## Features

- **Capture & pin.** Grab the full screen, a region, or a detected window;
  pin captures on top of your desktop for quick reference. (Region/window/pin
  arrive in later phases; fullscreen capture works today.)
- **OCR with table → Excel paste + LLM chat.** Recognize text in screenshots —
  including tables — and paste them straight into Excel as a grid, then ask an
  LLM follow-up questions about the captured content.
- **Translate to Chinese.** One-click translation of OCR output or arbitrary
  text to Simplified Chinese via an OpenAI-compatible model.

## Build

```sh
cargo build --release
```

The resulting binary on Windows is `target/release/screenshot-dai.exe`.

## CI

GitHub Actions (`.github/workflows/build.yml`) builds the Windows release
binary on every push/PR and uploads it as an artifact, with a fast Linux
`cargo check` job for early feedback.

## Status

Early development.
