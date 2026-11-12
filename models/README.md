# Models

## nsfw.onnx

The NSFW image scan's model (`crates/bot/src/nsfw.rs`), built into the bot binary with `include_bytes!`.

- Source: [GantMan/nsfw_model](https://github.com/GantMan/nsfw_model) release 1.1.0, `nsfw_mobilenet_v2_140_224.zip`, file `saved_model.tflite` (MobileNetV2 1.40, 224 x 224 input, values 0 to 1).
- Licence: MIT, Copyright (c) 2019 Gant Laborde.
- Classes, in output order: drawings, hentai, neutral, porn, sexy.
- Converted to ONNX once, because tract cannot load the TFLite file or the frozen graph:
  `python -m tf2onnx.convert --tflite saved_model.tflite --output nsfw.onnx --opset 13` (tf2onnx with tensorflow-cpu).
- SHA-256: `e448e86df0d976da123f746cbf4b926713a774064a69ce9b25cff04d58abadbb`. The test
  `nsfw::tests::bundled_model_is_the_pinned_one_and_scores_a_plain_image_neutral` checks it; a new file needs a new hash there.

Measured on one x86 core: about 77 ms per image, about 65 MB RAM with the model loaded.
