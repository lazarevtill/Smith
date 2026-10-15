import Darwin
import Foundation
import XCTest

@testable import WorkerCore

final class PackageTests: XCTestCase {
  var root: URL!
  var scratch: URL!
  override func setUpWithError() throws {
    // Foundation intentionally shortens /private/var back to the /var symlink on macOS.
    // POSIX realpath preserves the actual symlink-free authority spelling required by the worker.
    let resolved = try XCTUnwrap(realpath(FileManager.default.temporaryDirectory.path, nil))
    defer { free(resolved) }
    let base = URL(fileURLWithPath: String(cString: resolved))
    root = base.appendingPathComponent("smith-mlx-test-" + UUID().uuidString)
    scratch = root.appendingPathComponent("scratch")
    try FileManager.default.createDirectory(
      at: root, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
    try FileManager.default.createDirectory(
      at: scratch, withIntermediateDirectories: false, attributes: [.posixPermissions: 0o700])
  }
  override func tearDownWithError() throws { try FileManager.default.removeItem(at: root) }
  func model() throws -> URL {
    let model = root.appendingPathComponent("model")
    try FileManager.default.createDirectory(at: model, withIntermediateDirectories: false)
    try put(
      model, "config.json",
      [
        "model_type": "llama", "hidden_size": 64, "num_attention_heads": 4, "num_hidden_layers": 1,
        "intermediate_size": 128, "vocab_size": 256, "max_position_embeddings": 8192,
      ])
    try put(
      model, "tokenizer_config.json",
      ["chat_template": SafeTemplate.chatMLSource, "tokenizer_class": "PreTrainedTokenizerFast"])
    try put(model, "tokenizer.json", ["model": ["type": "BPE", "vocab": ["a": 0], "merges": []]])
    try weights(model)
    return model
  }
  func weights(
    _ model: URL,
    header: [String: Any] = [
      "test.weight": ["dtype": "F32", "shape": [1], "data_offsets": [0, 4]]
    ]
  ) throws {
    let json = try JSONSerialization.data(withJSONObject: header, options: [.sortedKeys])
    var size = UInt64(json.count).littleEndian
    var data = withUnsafeBytes(of: &size) { Data($0) }
    data.append(json)
    data.append(contentsOf: [0, 0, 0, 0])
    try data.write(to: model.appendingPathComponent("model.safetensors"))
  }
  func put(_ model: URL, _ name: String, _ object: [String: Any]) throws {
    try JSONSerialization.data(withJSONObject: object).write(to: model.appendingPathComponent(name))
  }
  func read(_ model: URL) throws -> LocalSnapshot {
    try LocalPackage.snapshot(root: model.path, scratch: scratch.path, cancelled: { false })
  }
  func testManifestSnapshotAndCleanup() throws {
    let model = try model()
    var snapshot: LocalSnapshot? = try read(model)
    let location = snapshot!.directory
    XCTAssertEqual(snapshot!.manifest.capabilities, ["raw_completion", "chat"])
    XCTAssertEqual(snapshot!.manifest.files.count, 4)
    XCTAssertEqual(snapshot!.manifest.package_sha256.count, 64)
    try Data("changed".utf8).write(to: model.appendingPathComponent("tokenizer_config.json"))
    XCTAssertTrue(
      try Data(contentsOf: location.appendingPathComponent("tokenizer_config.json")).count > 10)
    snapshot = nil
    XCTAssertFalse(FileManager.default.fileExists(atPath: location.path))
  }
  func testStableHashAndUnknownTemplateCapability() throws {
    let model = try model()
    XCTAssertEqual(try read(model).manifest.package_sha256, try read(model).manifest.package_sha256)
    try put(
      model, "tokenizer_config.json",
      ["chat_template": "{{ arbitrary_call() }}", "tokenizer_class": "PreTrainedTokenizerFast"])
    XCTAssertEqual(try read(model).manifest.capabilities, ["raw_completion"])
  }
  func testSymlinksHardlinksAndRootAliasesRejected() throws {
    let model = try model()
    let alias = root.appendingPathComponent("alias")
    try FileManager.default.createSymbolicLink(at: alias, withDestinationURL: model)
    for path in [alias.path, alias.path + "/", alias.path + "/.", model.path + "/../model"] {
      XCTAssertThrowsError(
        try LocalPackage.snapshot(root: path, scratch: scratch.path, cancelled: { false }))
    }
    let token = model.appendingPathComponent("tokenizer.json")
    let external = root.appendingPathComponent("external.json")
    try FileManager.default.moveItem(at: token, to: external)
    try FileManager.default.createSymbolicLink(at: token, withDestinationURL: external)
    XCTAssertThrowsError(try read(model))
    try FileManager.default.removeItem(at: token)
    XCTAssertEqual(link(external.path, token.path), 0)
    XCTAssertThrowsError(try read(model))
  }
  func testShardTraversalMismatchAndMissingFilesRejected() throws {
    let model = try model()
    try put(
      model, "model.safetensors.index.json",
      ["weight_map": ["test.weight": "../outside.safetensors"]])
    XCTAssertThrowsError(try read(model))
    try put(
      model, "model.safetensors.index.json", ["weight_map": ["test.weight": "model.safetensors"]])
    XCTAssertNoThrow(try read(model))
    try FileManager.default.removeItem(at: model.appendingPathComponent("tokenizer.json"))
    XCTAssertThrowsError(try read(model))
  }
  func testMalformedWeightsAndDimensionsRejectedBeforeMLX() throws {
    let model = try model()
    try weights(
      model,
      header: ["test.weight": ["dtype": "F32", "shape": [999_999_999], "data_offsets": [0, 4]]])
    XCTAssertThrowsError(try read(model))
    try weights(model)
    try put(model, "config.json", ["model_type": "llama", "hidden_size": 999_999_999])
    XCTAssertThrowsError(try read(model))
    try put(model, "config.json", ["model_type": "custom_remote"])
    XCTAssertThrowsError(try read(model))
  }
  func testCancellationAndScratchPermissions() throws {
    let model = try model()
    XCTAssertThrowsError(
      try LocalPackage.snapshot(root: model.path, scratch: scratch.path, cancelled: { true }))
    XCTAssertTrue(try FileManager.default.contentsOfDirectory(atPath: scratch.path).isEmpty)
    XCTAssertEqual(chmod(scratch.path, 0o755), 0)
    XCTAssertThrowsError(try read(model))
  }
  func testCustomCodeAndNestedPackageDenied() throws {
    let model = try model()
    try Data("pass".utf8).write(to: model.appendingPathComponent("modeling_custom.py"))
    XCTAssertThrowsError(try read(model))
    try FileManager.default.removeItem(at: model.appendingPathComponent("modeling_custom.py"))
    try FileManager.default.createDirectory(
      at: model.appendingPathComponent("subdir"), withIntermediateDirectories: false)
    XCTAssertThrowsError(try read(model))
  }
  func testDuplicateAffineQuantizationMustAgreeWithoutExtraKeys() throws {
    let model = try model()
    var config = try XCTUnwrap(
      JSONSerialization.jsonObject(with: Data(contentsOf: model.appendingPathComponent("config.json"))) as? [String: Any])
    config["quantization"] = ["bits": 4, "group_size": 64]
    config["quantization_config"] = ["bits": 4, "group_size": 64, "mode": "affine"]
    try put(model, "config.json", config)
    XCTAssertNoThrow(try read(model))
    for bad: Any in [
      ["bits": 8, "group_size": 64], ["bits": true, "group_size": 64],
      ["bits": 4.5, "group_size": 64], ["bits": 4, "group_size": 64, "mode": "mxfp4"],
      ["bits": 4, "group_size": 64, "quant_method": "custom"],
      ["bits": 4, "group_size": 64, "layer.0": ["bits": 8]], "not a dictionary",
    ] {
      config["quantization_config"] = bad
      try put(model, "config.json", config)
      XCTAssertThrowsError(try read(model))
    }
    config["quantization_config"] = ["bits": 4, "group_size": 64]
    config.removeValue(forKey: "quantization")
    try put(model, "config.json", config)
    XCTAssertThrowsError(try read(model))
    config["quantization"] = ["bits": 4, "group_size": 64, "layer.0": ["bits": 8]]
    config["quantization_config"] = config["quantization"]
    try put(model, "config.json", config)
    XCTAssertThrowsError(try read(model))
    config["quantization"] = NSNull()
    config["quantization_config"] = NSNull()
    try put(model, "config.json", config)
    XCTAssertNoThrow(try read(model))
  }
}
