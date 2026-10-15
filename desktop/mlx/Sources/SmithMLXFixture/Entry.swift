import Foundation
import MLX
import MLXLLM
import MLXNN
import Metal
import WorkerCore

// Test utility only, never shipped or invoked by the worker. All weights are created here.
// A one-layer Llama deterministically emits byte-token 65 ("A") at temperature zero.
@main
enum FixtureMain {
  static func main() throws {
    guard CommandLine.arguments.count == 2 else {
      throw WorkerError("fixture_usage", "Provide one new fixture output directory.")
    }
    let directory = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    guard directory.path.hasPrefix("/"),
      !FileManager.default.fileExists(atPath: directory.path),
      let device = MTLCreateSystemDefaultDevice()
    else {
      throw WorkerError(
        "fixture_unavailable", "A new output directory and Metal device are required.")
    }
    try FileManager.default.createDirectory(
      at: directory, withIntermediateDirectories: false,
      attributes: [.posixPermissions: 0o700])
    do {
      try Device.withDefaultDevice(.gpu) {
        Memory.cacheLimit = 32 * 1024 * 1024
        Memory.memoryLimit = 256 * 1024 * 1024
        let configuration = LlamaConfiguration(
          hiddenSize: 64, hiddenLayers: 1, intermediateSize: 128, attentionHeads: 4,
          rmsNormEps: 0.00001, vocabularySize: 258, kvHeads: 4,
          maxPositionEmbeddings: 8192, tieWordEmbeddings: false)
        let model = LlamaModel(configuration)
        let shapes = model.parameters().flattened()
        var arrays = [String: MLXArray]()
        for (name, parameter) in shapes {
          if name == "model.embed_tokens.weight" || name.hasSuffix("norm.weight") {
            arrays[name] = MLXArray.ones(parameter.shape)
          } else if name == "lm_head.weight" {
            var values = [Float](repeating: 0, count: 258 * 64)
            for index in (65 * 64)..<(66 * 64) { values[index] = 1 }
            arrays[name] = MLXArray(values, [258, 64])
          } else {
            arrays[name] = MLXArray.zeros(parameter.shape)
          }
        }
        try MLX.save(
          arrays: arrays, metadata: ["origin": "smith-synthetic-llama-constant-A-v1"],
          url: directory.appendingPathComponent("model.safetensors"))
        var config =
          try JSONSerialization.jsonObject(with: JSONEncoder().encode(configuration))
          as! [String: Any]
        config["model_type"] = "llama"
        config["eos_token_id"] = 257
        config["bos_token_id"] = 256
        try writeJSON(config, directory, "config.json")
        var vocab = [String: Int]()
        let direct = Set(Array(33...126) + Array(161...172) + Array(174...255))
        var extensionIndex = 0
        for byte in 0...255 {
          let scalar: Int
          if direct.contains(byte) {
            scalar = byte
          } else {
            scalar = 256 + extensionIndex
            extensionIndex += 1
          }
          vocab[String(UnicodeScalar(scalar)!)] = byte
        }
        vocab["<|im_start|>"] = 256
        vocab["<|im_end|>"] = 257
        let added: [[String: Any]] = [
          [
            "id": 256, "content": "<|im_start|>", "special": true,
            "single_word": false, "lstrip": false, "rstrip": false, "normalized": false,
          ],
          [
            "id": 257, "content": "<|im_end|>", "special": true,
            "single_word": false, "lstrip": false, "rstrip": false, "normalized": false,
          ],
        ]
        try writeJSON(
          [
            "version": "1.0", "added_tokens": added,
            "model": ["type": "BPE", "vocab": vocab, "merges": []] as [String: Any],
            "pre_tokenizer": [
              "type": "ByteLevel", "add_prefix_space": false,
              "trim_offsets": false, "use_regex": false,
            ],
            "decoder": [
              "type": "ByteLevel", "add_prefix_space": false,
              "trim_offsets": false, "use_regex": false,
            ],
          ], directory, "tokenizer.json")
        try writeJSON(
          [
            "tokenizer_class": "PreTrainedTokenizerFast", "bos_token": "<|im_start|>",
            "eos_token": "<|im_end|>", "chat_template": SafeTemplate.chatMLSource,
            "model_max_length": 8192,
          ], directory, "tokenizer_config.json")
        let report: [String: Any] = [
          "schema": 1, "fixture": "smith-synthetic-llama-constant-A-v1",
          "tensor_count": arrays.count, "metal_device": device.name,
          "mlx_device": Device.defaultDevice().deviceType == .gpu ? "gpu" : "cpu",
          "weights_downloaded": false,
        ]
        let output = try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys])
        FileHandle.standardOutput.write(output + Data([10]))
      }
    } catch {
      try? FileManager.default.removeItem(at: directory)
      throw error
    }
  }

  private static func writeJSON(_ value: [String: Any], _ directory: URL, _ name: String) throws {
    let bytes = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    try bytes.write(to: directory.appendingPathComponent(name), options: [.withoutOverwriting])
  }
}
