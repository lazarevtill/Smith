import Foundation
import Hub
import MLX
import MLXLLM
import MLXLMCommon
import Metal
import Tokenizers
import WorkerCore

// No HubApi, Downloader, repository identifier or network entrypoint is constructed.
private struct LocalTokenizerLoader: MLXLMCommon.TokenizerLoader {
  let expectedDirectory: URL
  func load(from directory: URL) async throws -> any MLXLMCommon.Tokenizer {
    guard directory == expectedDirectory else {
      throw WorkerError("unsafe_path", "Tokenizer directory differs from the selected snapshot.")
    }
    let configData = try Data(contentsOf: directory.appendingPathComponent("tokenizer_config.json"))
    let tokenData = try Data(contentsOf: directory.appendingPathComponent("tokenizer.json"))
    let config = try JSONDecoder().decode(Config.self, from: configData)
    let data = try JSONDecoder().decode(Config.self, from: tokenData)
    let tokenizer = try AutoTokenizer.from(
      tokenizerConfig: config, tokenizerData: data, strict: true)
    return LocalTokenizer(value: tokenizer)
  }
}
private struct LocalTokenizer: MLXLMCommon.Tokenizer {
  let value: any Tokenizers.Tokenizer
  func encode(text: String, addSpecialTokens: Bool) -> [Int] {
    value.encode(text: text, addSpecialTokens: addSpecialTokens)
  }
  func decode(tokenIds: [Int], skipSpecialTokens: Bool) -> String {
    value.decode(tokens: tokenIds, skipSpecialTokens: skipSpecialTokens)
  }
  func convertTokenToId(_ token: String) -> Int? { value.convertTokenToId(token) }
  func convertIdToToken(_ id: Int) -> String? { value.convertIdToToken(id) }
  var bosToken: String? { value.bosToken }
  var eosToken: String? { value.eosToken }
  var unknownToken: String? { value.unknownToken }
  func applyChatTemplate(
    messages: [[String: any Sendable]], tools: [[String: any Sendable]]?,
    additionalContext: [String: any Sendable]?
  ) throws -> [Int] {
    throw WorkerError(
      "unsupported_template",
      "Arbitrary template evaluation is disabled; use an admitted template or explicit raw mode.")
  }
}

actor NativeBackend: WorkerBackend {
  let scratch: String
  private var snapshot: LocalSnapshot?
  private var model: ModelContainer?
  init(scratch: String) { self.scratch = scratch }
  func inspect(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  {
    let snapshot = try LocalPackage.snapshot(
      root: root, scratch: scratch, cancelled: { cancellation.isCancelled })
    try Self.checkHash(expectedHash, snapshot.manifest)
    return snapshot.manifest
  }
  func load(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  {
    guard model == nil else {
      throw WorkerError(
        "model_loaded", "Unload the current model before selecting another package.")
    }
    let snapshot = try LocalPackage.snapshot(
      root: root, scratch: scratch, cancelled: { cancellation.isCancelled })
    try cancellation.check()
    guard let expectedHash else { throw WorkerError.invalid }
    try Self.checkHash(expectedHash, snapshot.manifest)
    guard let executable = Bundle.main.executableURL else {
      throw WorkerError(
        "invalid_runtime_resources", "The native executable location could not be established.")
    }
    let library = try RuntimeResources.validate(directory: executable.deletingLastPathComponent())
    // Cmlx tries this colocated file first. Validate that exact file before MLX initialization:
    // a corrupt local library must not trigger its upstream bundle/build-directory fallbacks.
    guard let device = MTLCreateSystemDefaultDevice() else {
      throw WorkerError("metal_unavailable", "A native Metal device is unavailable.")
    }
    do { _ = try device.makeLibrary(URL: library) } catch {
      throw WorkerError("invalid_metallib", "The reviewed local Metal library could not be loaded.")
    }
    try cancellation.check()
    // These setters are reached only after an explicit load request, never during hello/inspect/tests.
    Memory.cacheLimit = 256 * 1024 * 1024
    Memory.memoryLimit = 16 * 1024 * 1024 * 1024
    let loaded = try await LLMModelFactory.shared.loadContainer(
      from: snapshot.directory, using: LocalTokenizerLoader(expectedDirectory: snapshot.directory))
    try cancellation.check()
    self.snapshot = snapshot
    self.model = loaded
    return snapshot.manifest
  }
  private static func checkHash(_ expected: String?, _ manifest: PackageManifest) throws {
    if let expected, expected != manifest.package_sha256 {
      throw WorkerError(
        "package_changed", "The local package differs from the reviewed content hash.")
    }
  }
  func generate(
    _ request: Request, cancellation: Cancellation,
    update: @escaping @Sendable (Int, String) -> Void
  ) async throws -> Completion {
    guard let model, let snapshot else {
      throw WorkerError("no_model", "Load an explicitly selected local model first.")
    }
    try cancellation.check()
    let prompt: String
    if request.mode == "chat" {
      guard let template = snapshot.template else {
        throw WorkerError(
          "unsupported_template",
          "This local template is outside the bounded chat subset. Explicit raw completion remains available."
        )
      }
      prompt = try template.render(
        request.messages!, thinkingMode: request.thinking_mode ?? .modelDefault)
    } else {
      prompt = request.prompt!
    }
    return try await model.perform { context in
      let tokens = context.tokenizer.encode(text: prompt, addSpecialTokens: false)
      guard !tokens.isEmpty, tokens.count <= Limits.inputTokens,
        tokens.count + request.max_tokens! <= snapshot.manifest.context_limit
      else {
        throw WorkerError(
          "context_limit",
          "The complete prompt and requested output exceed the model context budget. No input was truncated."
        )
      }
      try cancellation.check()
      let parameters = GenerateParameters(
        maxTokens: request.max_tokens!, maxKVSize: snapshot.manifest.context_limit,
        temperature: request.temperature!, topP: request.top_p!, seed: request.seed)
      let input = LMInput(tokens: MLXArray(tokens))
      let (stream, producer) = try generateTokensTask(
        input: input, parameters: parameters, context: context)
      return try await withTaskCancellationHandler {
        var generated = [Int]()
        var previous = ""
        var finish = "stop"
        do {
          for await event in stream {
            try cancellation.check()
            switch event {
            case .token(let token):
              guard generated.count < request.max_tokens! else {
                throw WorkerError("output_limit", "The model exceeded the generation token budget.")
              }
              generated.append(token)
              let text = context.tokenizer.decode(tokenIds: generated, skipSpecialTokens: false)
              if !text.hasSuffix("\u{fffd}") {
                let patch = try TextPatch.difference(previous: previous, current: text)
                if !patch.1.isEmpty || patch.0 != previous.utf8.count { update(patch.0, patch.1) }
                previous = text
              }
            case .info(let info):
              switch info.stopReason {
              case .stop: finish = "stop"
              case .length: finish = "length"
              case .cancelled: finish = "cancelled"
              }
            }
          }
          await producer.value
          try cancellation.check()
          let final = context.tokenizer.decode(tokenIds: generated, skipSpecialTokens: false)
          let patch = try TextPatch.difference(previous: previous, current: final)
          if !patch.1.isEmpty || patch.0 != previous.utf8.count { update(patch.0, patch.1) }
          return Completion(
            inputTokens: tokens.count, outputTokens: generated.count, reason: finish,
            thinkingMode: request.thinking_mode)
        } catch {
          producer.cancel()
          await producer.value
          throw error
        }
      } onCancel: {
        producer.cancel()
      }
    }
  }
  func unload() async {
    model = nil
    snapshot = nil
  }
}
