import CryptoKit
import Darwin
import Foundation

public struct FileRecord: Codable, Sendable {
  public let name: String
  public let bytes: Int64
  public let sha256: String
}
public struct PackageManifest: Codable, Sendable {
  public let model_type: String
  public let files: [FileRecord]
  public let package_sha256: String
  public let template: String?
  public let context_limit: Int
  public var capabilities: [String] {
    template == nil ? ["raw_completion"] : ["raw_completion", "chat"]
  }
}
public final class LocalSnapshot: @unchecked Sendable {
  public let directory: URL
  public let manifest: PackageManifest
  public let template: SafeTemplate?
  init(directory: URL, manifest: PackageManifest, template: SafeTemplate?) {
    self.directory = directory
    self.manifest = manifest
    self.template = template
  }
  deinit { try? FileManager.default.removeItem(at: directory) }
}

public enum LocalPackage {
  private static let smallFiles: Set<String> = [
    "config.json", "tokenizer.json", "tokenizer_config.json", "generation_config.json",
    "model.safetensors.index.json", "special_tokens_map.json", "added_tokens.json",
    "chat_template.jinja",
  ]
  public static func openDirectory(_ path: String) throws -> Int32 {
    let parts = path.split(separator: "/", omittingEmptySubsequences: false)
    guard path.hasPrefix("/"), path != "/", path.utf8.count <= 4096, !path.contains("\0"),
      parts.dropFirst().allSatisfy({ !$0.isEmpty && $0 != "." && $0 != ".." })
    else {
      throw WorkerError("unsafe_path", "Use an absolute path without aliases or dot components.")
    }
    var fd = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
    guard fd >= 0 else { throw ioError() }
    for part in parts.dropFirst() {
      let next = openat(fd, String(part), O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
      close(fd)
      guard next >= 0 else {
        throw WorkerError(
          "unsafe_path",
          "The selected directory must exist and contain no symbolic-link components.")
      }
      fd = next
    }
    return fd
  }
  private static func ioError() -> WorkerError {
    WorkerError("package_io", "The selected local package could not be read safely.")
  }
  public static func snapshot(root: String, scratch: String, cancelled: @Sendable () -> Bool) throws
    -> LocalSnapshot
  {
    let source = try openDirectory(root)
    defer { close(source) }
    let scratchFD = try openDirectory(scratch)
    defer { close(scratchFD) }
    var scratchInfo = stat()
    guard fstat(scratchFD, &scratchInfo) == 0, scratchInfo.st_uid == geteuid(),
      scratchInfo.st_mode & 0o077 == 0
    else {
      throw WorkerError(
        "unsafe_scratch", "Worker scratch must be owned by this user and private (0700).")
    }
    let name = "snapshot-" + UUID().uuidString.lowercased()
    guard mkdirat(scratchFD, name, 0o700) == 0 else { throw ioError() }
    let destinationURL = URL(fileURLWithPath: scratch).appendingPathComponent(name)
    var retained = false
    defer { if !retained { try? FileManager.default.removeItem(at: destinationURL) } }
    let destination = openat(scratchFD, name, O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC)
    guard destination >= 0 else { throw ioError() }
    defer { close(destination) }
    guard let directory = fdopendir(dup(source)) else { throw ioError() }
    defer { closedir(directory) }
    var names = [String]()
    while let entry = readdir(directory) {
      let item = withUnsafePointer(to: entry.pointee.d_name) { pointer in
        pointer.withMemoryRebound(to: CChar.self, capacity: 1024) { String(cString: $0) }
      }
      if item == "." || item == ".." { continue }
      guard names.count < Limits.files else {
        throw WorkerError(
          "resource_limit", "The model package contains too many directory entries.")
      }
      names.append(item)
    }
    var records = [FileRecord]()
    var json = [String: Data]()
    var total: Int64 = 0
    var tensorFiles = [String: String]()
    for name in names.sorted() {
      if cancelled() { throw WorkerError.cancelled }
      guard name.utf8.count <= 200, !name.contains("/"), !name.contains("\\"), !name.contains("\0")
      else { throw ioError() }
      let isWeight = name.hasSuffix(".safetensors")
      // Ignore only passive descriptive files. Directories, code and unknown data are rejected.
      let ignored =
        name.hasSuffix(".md")
        || ["LICENSE", "LICENSE.txt", ".gitattributes", ".DS_Store"].contains(name)
      guard isWeight || smallFiles.contains(name) || ignored else {
        throw WorkerError(
          "unsupported_package", "The selected package contains an unsupported file or directory.")
      }
      let input = openat(source, name, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
      guard input >= 0 else { throw ioError() }
      defer { close(input) }
      var before = stat()
      guard fstat(input, &before) == 0, before.st_mode & S_IFMT == S_IFREG, before.st_nlink == 1,
        before.st_size >= 0
      else {
        throw WorkerError(
          "unsafe_path", "Model files must be regular files without symbolic or hard links.")
      }
      if ignored { continue }
      let cap: Int64 =
        isWeight
        ? Limits.weightsBytes : Int64(name == "tokenizer.json" ? Limits.jsonBytes : 1_048_576)
      guard before.st_size <= cap, total <= Limits.weightsBytes - before.st_size else {
        throw WorkerError("resource_limit", "The selected model package exceeds its size limit.")
      }
      total += before.st_size
      if isWeight {
        guard
          name.utf8.allSatisfy({
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0)
              || [45, 46, 95].contains($0)
          })
        else { throw ioError() }
        for tensor in try inspectTensors(input, size: before.st_size) {
          guard tensorFiles.updateValue(name, forKey: tensor) == nil else {
            throw WorkerError(
              "invalid_weights", "Weight tensor names must be unique across shards.")
          }
        }
      }
      let output = openat(
        destination, name, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600)
      guard output >= 0 else { throw ioError() }
      defer { close(output) }
      guard lseek(input, 0, SEEK_SET) == 0 else { throw ioError() }
      var hash = SHA256()
      var count: Int64 = 0
      var small = Data()
      var buffer = [UInt8](repeating: 0, count: 1_048_576)
      while true {
        if cancelled() { throw WorkerError.cancelled }
        let n = read(input, &buffer, buffer.count)
        if n < 0 {
          if errno == EINTR { continue }
          throw ioError()
        }
        if n == 0 { break }
        guard count <= before.st_size - Int64(n) else {
          throw WorkerError("package_changed", "The selected package changed while being read.")
        }
        count += Int64(n)
        let chunk = Data(buffer[..<n])
        hash.update(data: chunk)
        if !isWeight { small.append(chunk) }
        try chunk.withUnsafeBytes { raw in
          var offset = 0
          while offset < n {
            let written = write(output, raw.baseAddress!.advanced(by: offset), n - offset)
            if written < 0 && errno == EINTR { continue }
            guard written > 0 else { throw ioError() }
            offset += written
          }
        }
      }
      var after = stat()
      guard count == before.st_size, fstat(input, &after) == 0, before.st_size == after.st_size,
        before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec,
        before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
        before.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec,
        before.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec
      else {
        throw WorkerError("package_changed", "The selected package changed while being read.")
      }
      guard fchmod(output, 0o400) == 0 else { throw ioError() }
      if !isWeight { json[name] = small }
      records.append(.init(name: name, bytes: count, sha256: hex(hash.finalize())))
    }
    guard let configData = json["config.json"], let tokenizerData = json["tokenizer.json"],
      let tokenizerConfigData = json["tokenizer_config.json"], !tensorFiles.isEmpty
    else {
      throw WorkerError(
        "incomplete_package",
        "Local config, tokenizer, tokenizer config and safetensors weights are required.")
    }
    let config = try BoundedJSON.object(configData)
    let modelType = try validateConfig(config)
    let tokenizer = try BoundedJSON.object(tokenizerData)
    let tokenizerConfig = try BoundedJSON.object(tokenizerConfigData)
    try TokenizerPolicy.validate(
      data: tokenizer, config: tokenizerConfig, vocabularyLimit: config["vocab_size"] as! Int)
    if let indexData = json["model.safetensors.index.json"] {
      let index = try BoundedJSON.object(indexData)
      guard let map = index["weight_map"] as? [String: String], map == tensorFiles else {
        throw WorkerError(
          "invalid_weights",
          "The shard index must exactly match local tensor names and top-level shard files.")
      }
    }
    for (name, data) in json where name.hasSuffix(".json") { _ = try BoundedJSON.object(data) }
    let configTemplate = tokenizerConfig["chat_template"] as? String
    let fileTemplate = json["chat_template.jinja"].flatMap { String(data: $0, encoding: .utf8) }
    if json["chat_template.jinja"] != nil && fileTemplate == nil {
      throw WorkerError("invalid_template", "The local chat template must be UTF-8.")
    }
    if let configTemplate, let fileTemplate, configTemplate != fileTemplate {
      throw WorkerError("invalid_template", "Conflicting local chat templates are not accepted.")
    }
    let template = (fileTemplate ?? configTemplate).flatMap(SafeTemplate.init(source:))
    var fingerprint = SHA256()
    for record in records {
      fingerprint.update(data: Data("\(record.name)\0\(record.bytes)\0\(record.sha256)\n".utf8))
    }
    let context = min(
      Limits.inputTokens + Limits.outputTokens,
      config["max_position_embeddings"] as? Int ?? Limits.inputTokens)
    let manifest = PackageManifest(
      model_type: modelType, files: records, package_sha256: hex(fingerprint.finalize()),
      template: template?.rawValue, context_limit: context)
    retained = true
    return LocalSnapshot(directory: destinationURL, manifest: manifest, template: template)
  }
  private static func hex<D: Sequence>(_ bytes: D) -> String where D.Element == UInt8 {
    bytes.map { String(format: "%02x", $0) }.joined()
  }
  private static func validateConfig(_ config: [String: Any]) throws -> String {
    guard let type = config["model_type"] as? String,
      ["llama", "mistral", "qwen2", "qwen3", "granite"].contains(type),
      config["auto_map"] == nil, config["vision_config"] == nil, config["audio_config"] == nil,
      config["num_local_experts"] == nil
    else {
      throw WorkerError(
        "unsupported_model", "Only reviewed dense local text architectures are supported.")
    }
    func number(_ key: String, _ range: ClosedRange<Int>, fallback: Int? = nil) throws -> Int {
      guard let value = config[key] as? NSNumber ?? fallback.map(NSNumber.init),
        CFGetTypeID(value) != CFBooleanGetTypeID(),
        value.doubleValue == Double(value.intValue), range.contains(value.intValue)
      else {
        throw WorkerError(
          "invalid_config", "Model dimensions are missing or outside the supported bounds.")
      }
      return value.intValue
    }
    for key in ["rope_scaling", "rope_parameters"] {
      if let value = config[key], !(value is NSNull) {
        throw WorkerError(
          "unsupported_model",
          "Custom rotary scaling or alternate quantization metadata is outside this model profile.")
      }
    }
    let hidden = try number("hidden_size", 64...8192)
    let heads = try number("num_attention_heads", 1...128)
    let kv = try number("num_key_value_heads", 1...128, fallback: heads)
    let layers = try number("num_hidden_layers", 1...80)
    let intermediate = try number("intermediate_size", 64...32768)
    let vocab = try number("vocab_size", 1...262144)
    _ = try number("max_position_embeddings", 64...1_048_576, fallback: 8192)
    let headDim = try number("head_dim", 8...256, fallback: hidden / heads)
    guard hidden % heads == 0, heads % kv == 0, headDim % 2 == 0,
      Int64(layers) * Int64(4 * hidden * hidden + 3 * hidden * intermediate)
        + Int64(2 * vocab * hidden) <= 12_000_000_000
    else {
      throw WorkerError(
        "resource_limit", "Model dimensions exceed the worker's dense-model budget.")
    }
    func quantization(_ value: Any) throws -> (Int, Int) {
      func integer(_ value: Any?) -> Int? {
        guard let n = value as? NSNumber, CFGetTypeID(n) != CFBooleanGetTypeID(),
          n.doubleValue == Double(n.intValue)
        else { return nil }
        return n.intValue
      }
      guard let quant = value as? [String: Any],
        Set(quant.keys).isSubset(of: ["bits", "group_size", "mode"]),
        let bits = integer(quant["bits"]), [4, 8].contains(bits),
        let group = integer(quant["group_size"]), [32, 64, 128].contains(group),
        quant["mode"] == nil || quant["mode"] as? String == "affine"
      else {
        throw WorkerError(
          "unsupported_quantization", "Only explicit affine 4-bit or 8-bit quantization is supported.")
      }
      return (bits, group)
    }
    let primaryValue = config["quantization"].flatMap { $0 is NSNull ? nil : $0 }
    let primary = try primaryValue.map(quantization)
    if let alias = config["quantization_config"], !(alias is NSNull) {
      guard let primary, try quantization(alias) == primary else {
        throw WorkerError(
          "unsupported_quantization", "Duplicate quantization metadata must match the selected affine format.")
      }
    }
    return type
  }
  private static func inspectTensors(_ fd: Int32, size: Int64) throws -> [String] {
    var prefix = [UInt8](repeating: 0, count: 8)
    guard pread(fd, &prefix, 8, 0) == 8 else {
      throw WorkerError("invalid_weights", "Safetensors header is incomplete.")
    }
    let headerSize = prefix.enumerated().reduce(UInt64(0)) {
      $0 | UInt64($1.element) << ($1.offset * 8)
    }
    guard headerSize > 1, headerSize <= 16_777_216, headerSize <= UInt64(max(0, size - 8)) else {
      throw WorkerError("invalid_weights", "Safetensors header exceeds its bounds.")
    }
    var data = Data(count: Int(headerSize))
    let readCount = data.withUnsafeMutableBytes { pread(fd, $0.baseAddress, $0.count, 8) }
    guard readCount == data.count else { throw ioError() }
    let object = try BoundedJSON.object(data)
    guard object.count <= 50_001 else {
      throw WorkerError("resource_limit", "Too many weight tensors.")
    }
    let widths = [
      "F64": 8, "F32": 4, "F16": 2, "BF16": 2, "I64": 8, "I32": 4, "U32": 4, "I16": 2, "U16": 2,
      "I8": 1, "U8": 1, "BOOL": 1,
    ]
    var intervals = [(Int64, Int64)]()
    var names = [String]()
    for (name, value) in object where name != "__metadata__" {
      guard name.utf8.count <= 1024, let tensor = value as? [String: Any],
        let dtype = tensor["dtype"] as? String, let width = widths[dtype],
        let shape = tensor["shape"] as? [Int], shape.count <= 4,
        let offsets = tensor["data_offsets"] as? [Int64], offsets.count == 2,
        offsets[0] >= 0, offsets[1] >= offsets[0], offsets[1] <= size - 8 - Int64(headerSize)
      else {
        throw WorkerError(
          "invalid_weights", "A safetensors tensor has invalid dimensions, type or offsets.")
      }
      var elements: Int64 = 1
      for dim in shape {
        guard (1...262144).contains(dim), elements <= Limits.weightsBytes / Int64(dim) else {
          throw WorkerError("resource_limit", "A tensor exceeds the dimension budget.")
        }
        elements *= Int64(dim)
      }
      guard elements <= Limits.weightsBytes / Int64(width),
        elements * Int64(width) == offsets[1] - offsets[0]
      else {
        throw WorkerError(
          "invalid_weights", "Safetensors byte counts do not match tensor dimensions.")
      }
      intervals.append((offsets[0], offsets[1]))
      names.append(name)
    }
    var end: Int64 = 0
    for interval in intervals.sorted(by: { $0.0 < $1.0 }) {
      guard interval.0 == end else {
        throw WorkerError("invalid_weights", "Safetensors ranges overlap or leave holes.")
      }
      end = interval.1
    }
    guard end == size - 8 - Int64(headerSize) else {
      throw WorkerError("invalid_weights", "Safetensors contains unaccounted bytes.")
    }
    return names
  }
}
