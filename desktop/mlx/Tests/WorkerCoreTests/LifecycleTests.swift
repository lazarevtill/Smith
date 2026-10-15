import Darwin
import Foundation
import XCTest

@testable import WorkerCore

private actor SyntheticBackend: WorkerBackend {
  private(set) var started = false
  private(set) var drained = false
  private(set) var unloaded = false
  func inspect(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  {
    PackageManifest(
      model_type: "synthetic-only", files: [], package_sha256: String(repeating: "0", count: 64),
      template: nil, context_limit: 8192)
  }
  func load(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  { try await inspect(root: root, expectedHash: expectedHash, cancellation: cancellation) }
  func generate(
    _ request: Request, cancellation: Cancellation,
    update: @escaping @Sendable (Int, String) -> Void
  ) async throws -> Completion {
    started = true
    defer { drained = true }
    update(0, "synthetic")
    while true {
      try cancellation.check()
      try await Task.sleep(for: .milliseconds(5))
    }
  }
  func unload() async { unloaded = true }
}

private actor StuckBackend: WorkerBackend {
  private var continuation: CheckedContinuation<Void, Never>?
  var started: Bool { continuation != nil }
  func inspect(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  { throw WorkerError.invalid }
  func load(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  { throw WorkerError.invalid }
  func generate(
    _ request: Request, cancellation: Cancellation,
    update: @escaping @Sendable (Int, String) -> Void
  ) async throws -> Completion {
    // Intentionally ignores task cancellation, modelling a native call that cannot be interrupted.
    await withCheckedContinuation { continuation = $0 }
    return Completion(inputTokens: 1, outputTokens: 0, reason: "stop")
  }
  func release() { continuation?.resume(); continuation = nil }
  func unload() async {}
}

private actor TemplateFixtureBackend: WorkerBackend {
  let omitAcknowledgement: Bool
  init(omitAcknowledgement: Bool = false) { self.omitAcknowledgement = omitAcknowledgement }
  func inspect(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  { throw WorkerError.invalid }
  func load(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  { throw WorkerError.invalid }
  func generate(
    _ request: Request, cancellation: Cancellation,
    update: @escaping @Sendable (Int, String) -> Void
  ) async throws -> Completion {
    // Pure template/protocol fixture: no tokenizer, model or GPU is invoked.
    _ = try SafeTemplate.qwen3.render(
      request.messages!, thinkingMode: request.thinking_mode ?? .modelDefault)
    try cancellation.check()
    update(0, "<think>\n\n</think>\n\nLiteral fixture response.")
    return Completion(
      inputTokens: 1, outputTokens: 1, reason: "stop",
      thinkingMode: omitAcknowledgement ? nil : request.thinking_mode)
  }
  func unload() async {}
}

private final class ExitRecorder: @unchecked Sendable {
  private let lock = NSLock()
  private var recorded: [Int32] = []
  func record(_ code: Int32) { lock.lock(); recorded.append(code); lock.unlock() }
  var codes: [Int32] { lock.lock(); defer { lock.unlock() }; return recorded }
}

@MainActor final class LifecycleTests: XCTestCase {
  private func request(_ id: String, _ op: String, _ extra: [String: Any] = [:]) throws -> Request {
    var object = extra
    object["v"] = 1
    object["id"] = id
    object["op"] = op
    return try Request.decode(JSONSerialization.data(withJSONObject: object))
  }
  private func generation(_ id: String) throws -> Request {
    try request(
      id, "generate",
      ["mode": "raw", "prompt": "test", "max_tokens": 10, "temperature": 0, "top_p": 1])
  }
  private func events(_ file: URL) throws -> [[String: Any]] {
    try Data(contentsOf: file).split(separator: 10).map {
      try JSONSerialization.jsonObject(with: Data($0)) as! [String: Any]
    }
  }
  private func capture() throws -> (URL, Int32) {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent(
      "smith-mlx-events-" + UUID().uuidString)
    let fd = open(url.path, O_CREAT | O_EXCL | O_WRONLY, 0o600)
    guard fd >= 0 else { throw WorkerError.invalid }
    return (url, fd)
  }
  func testCancelOwnIDAndTargetTerminalDrain() async throws {
    let (url, fd) = try capture()
    defer {
      close(fd)
      try? FileManager.default.removeItem(at: url)
    }
    let backend = SyntheticBackend()
    let worker = Worker(backend: backend, sink: EventSink(fd: fd), hardDeadlines: false)
    _ = await worker.accept(try generation("g1"))
    for _ in 0..<100 {
      if await backend.started { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    _ = await worker.accept(try request("busy", "inspect", ["model_root": "/synthetic"]))
    _ = await worker.accept(try request("cancel", "cancel", ["target_id": "g1"]))
    await worker.eof()
    let drained = await backend.drained
    let unloaded = await backend.unloaded
    XCTAssertTrue(drained)
    XCTAssertTrue(unloaded)
    let all = try events(url)
    XCTAssertEqual(
      all.filter { $0["id"] as? String == "g1" && $0["event"] as? String == "error" }.count, 1)
    XCTAssertEqual(all.first { $0["id"] as? String == "busy" }?["code"] as? String, "busy")
    XCTAssertEqual(
      all.first { $0["id"] as? String == "cancel" }?["cancel_requested"] as? Bool, true)
    XCTAssertEqual(all.last { $0["id"] as? String == "g1" }?["code"] as? String, "cancelled")
  }
  func testEOFCancelsAndDrainsWithoutExplicitCancel() async throws {
    let (url, fd) = try capture()
    defer {
      close(fd)
      try? FileManager.default.removeItem(at: url)
    }
    let backend = SyntheticBackend()
    let worker = Worker(backend: backend, sink: EventSink(fd: fd), hardDeadlines: false)
    _ = await worker.accept(try generation("g1"))
    for _ in 0..<100 {
      if await backend.started { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    await worker.eof()
    let drained = await backend.drained
    XCTAssertTrue(drained)
    XCTAssertEqual(try events(url).last?["code"] as? String, "cancelled")
  }
  func testDuplicateIDClosesSessionAndHelloDoesNotLoad() async throws {
    let (url, fd) = try capture()
    defer {
      close(fd)
      try? FileManager.default.removeItem(at: url)
    }
    let backend = SyntheticBackend()
    let worker = Worker(backend: backend, sink: EventSink(fd: fd), hardDeadlines: false)
    let first = await worker.accept(try request("hello", "hello"))
    XCTAssertTrue(first)
    let duplicate = await worker.accept(try request("hello", "hello"))
    XCTAssertFalse(duplicate)
    await worker.eof()
    let all = try events(url)
    XCTAssertEqual(all.first?["backend"] as? String, "mlx-swift-metal")
    XCTAssertEqual(
      all.first?["capabilities"] as? [String],
      ["inspect", "local_text", "qwen3_thinking_control_v1"])
    XCTAssertEqual(all.last?["code"] as? String, "duplicate_id")
    let started = await backend.started
    XCTAssertFalse(started)
  }
  func testExplicitCancelEscalatesStuckBackendOnDrainDeadline() async throws {
    let (url, fd) = try capture()
    defer { close(fd); try? FileManager.default.removeItem(at: url) }
    let backend = StuckBackend()
    let exits = ExitRecorder()
    let worker = Worker(
      backend: backend, sink: EventSink(fd: fd), drainDeadline: .milliseconds(20),
      terminate: { exits.record($0) })
    _ = await worker.accept(try generation("stuck"))
    for _ in 0..<100 {
      if await backend.started { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    _ = await worker.accept(try request("cancel-stuck", "cancel", ["target_id": "stuck"]))
    for _ in 0..<100 {
      if !exits.codes.isEmpty { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    XCTAssertEqual(exits.codes, [124])
    XCTAssertEqual(
      try events(url).first { $0["id"] as? String == "cancel-stuck" }?["event"] as? String,
      "done")
    await backend.release()
    await worker.eof()
    XCTAssertEqual(try events(url).last?["code"] as? String, "cancelled")
  }
  func testExplicitThinkingAcknowledgementAndLiteralOutputWithoutDefaultWireChanges() async throws {
    for mode: String? in [nil, "model_default", "disabled"] {
      let (url, fd) = try capture()
      defer { close(fd); try? FileManager.default.removeItem(at: url) }
      let worker = Worker(
        backend: TemplateFixtureBackend(), sink: EventSink(fd: fd), hardDeadlines: false)
      var extra: [String: Any] = [
        "mode": "chat", "messages": [["role": "user", "content": "literal /no_think"]],
        "max_tokens": 10, "temperature": 0, "top_p": 1,
      ]
      if let mode { extra["thinking_mode"] = mode }
      _ = await worker.accept(try request("generate", "generate", extra))
      for _ in 0..<100 {
        if try events(url).last?["event"] as? String == "done" { break }
        try await Task.sleep(for: .milliseconds(5))
      }
      await worker.eof()
      let all = try events(url)
      XCTAssertEqual(all.last?["event"] as? String, "done")
      XCTAssertEqual(all.last?["thinking_mode"] as? String, mode)
      XCTAssertEqual(
        all.first { $0["event"] as? String == "delta" }?["text"] as? String,
        "<think>\n\n</think>\n\nLiteral fixture response.")
      if mode == nil {
        XCTAssertEqual(
          Set(try XCTUnwrap(all.last).keys),
          ["v", "id", "event", "input_tokens", "output_tokens", "finish_reason"])
      }
    }
  }
  func testMissingBackendThinkingAcknowledgementCannotProduceSuccessfulTerminal() async throws {
    let (url, fd) = try capture()
    defer { close(fd); try? FileManager.default.removeItem(at: url) }
    let worker = Worker(
      backend: TemplateFixtureBackend(omitAcknowledgement: true), sink: EventSink(fd: fd),
      hardDeadlines: false)
    _ = await worker.accept(try request("generate", "generate", [
      "mode": "chat", "messages": [["role": "user", "content": "hello"]],
      "thinking_mode": "disabled", "max_tokens": 10, "temperature": 0, "top_p": 1,
    ]))
    for _ in 0..<100 {
      if try events(url).last?["event"] as? String == "error" { break }
      try await Task.sleep(for: .milliseconds(5))
    }
    await worker.eof()
    let all = try events(url)
    XCTAssertEqual(all.last?["code"] as? String, "invalid_completion")
    XCTAssertFalse(all.contains { $0["event"] as? String == "done" })
  }
}
