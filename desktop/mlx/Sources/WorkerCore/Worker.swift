import Darwin
import Foundation

public final class Cancellation: @unchecked Sendable {
  private let lock = NSLock()
  private var value = false
  public init() {}
  public var isCancelled: Bool {
    lock.lock()
    defer { lock.unlock() }
    return value
  }
  public func cancel() {
    lock.lock()
    value = true
    lock.unlock()
  }
  public func check() throws { if isCancelled || Task.isCancelled { throw WorkerError.cancelled } }
}
public struct Completion: Sendable {
  public let inputTokens: Int
  public let outputTokens: Int
  public let reason: String
  public let thinkingMode: ThinkingMode?
  public init(
    inputTokens: Int, outputTokens: Int, reason: String, thinkingMode: ThinkingMode? = nil
  ) {
    self.inputTokens = inputTokens
    self.outputTokens = outputTokens
    self.reason = reason
    self.thinkingMode = thinkingMode
  }
}
public protocol WorkerBackend: Sendable {
  func inspect(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  func load(root: String, expectedHash: String?, cancellation: Cancellation) async throws
    -> PackageManifest
  func generate(
    _ request: Request, cancellation: Cancellation,
    update: @escaping @Sendable (Int, String) -> Void
  ) async throws -> Completion
  func unload() async
}

private enum Outcome: Sendable {
  case manifest(PackageManifest)
  case completion(Completion)
  case unloaded
  case error(WorkerError)
}

public actor Worker {
  private let backend: any WorkerBackend
  private let sink: EventSink
  private var seen = Set<String>()
  private var active: (id: String, cancel: Cancellation, task: Task<Void, Never>)?
  private var closing = false
  private let hardDeadlines: Bool
  private let drainDeadline: Duration
  private let terminate: @Sendable (Int32) -> Void
  public init(backend: any WorkerBackend, sink: EventSink, hardDeadlines: Bool = true) {
    self.backend = backend
    self.sink = sink
    self.hardDeadlines = hardDeadlines
    self.drainDeadline = .seconds(5)
    self.terminate = { _exit($0) }
  }
  // Internal test seam: production always uses the fixed five-second process-exit boundary.
  init(
    backend: any WorkerBackend, sink: EventSink, drainDeadline: Duration,
    terminate: @escaping @Sendable (Int32) -> Void
  ) {
    self.backend = backend
    self.sink = sink
    self.hardDeadlines = true
    self.drainDeadline = drainDeadline
    self.terminate = terminate
  }

  public func accept(_ request: Request) async -> Bool {
    guard !closing else { return false }
    guard seen.count < Limits.requests else {
      sink.error(
        id: request.id, WorkerError("request_limit", "Restart the worker after its request limit."))
      return false
    }
    guard seen.insert(request.id).inserted else {
      sink.error(
        id: nil, WorkerError("duplicate_id", "Request IDs cannot be reused in a worker session."))
      return false
    }
    switch request.op {
    case "hello":
      sink.send(
        id: request.id, event: "done",
        fields: [
          "worker": "smith-mlx-worker", "protocol": 1, "backend": "mlx-swift-metal",
          "capabilities": ["inspect", "local_text", "qwen3_thinking_control_v1"],
          "max_frame_bytes": Limits.frameBytes,
          "max_input_tokens": Limits.inputTokens, "max_output_tokens": Limits.outputTokens,
        ])
    case "cancel":
      if let active, active.id == request.target_id {
        active.cancel.cancel()
        active.task.cancel()
        scheduleDrainDeadline(id: active.id)
        sink.send(
          id: request.id, event: "done", fields: ["target_id": active.id, "cancel_requested": true])
      } else {
        sink.error(
          id: request.id, WorkerError("not_active", "The cancellation target is not active."))
      }
    case "shutdown":
      closing = true
      await drain()
      sink.send(id: request.id, event: "done")
      return false
    default:
      guard active == nil else {
        sink.error(id: request.id, WorkerError("busy", "One operation may run at a time."))
        return true
      }
      let cancellation = Cancellation()
      let backend = self.backend
      let sink = self.sink
      sink.send(id: request.id, event: "started")
      let task = Task.detached { [weak self] in
        let outcome: Outcome
        do {
          switch request.op {
          case "inspect", "load":
            let manifest =
              try await request.op == "inspect"
              ? backend.inspect(
                root: request.model_root!, expectedHash: request.expected_package_sha256,
                cancellation: cancellation)
              : backend.load(
                root: request.model_root!, expectedHash: request.expected_package_sha256,
                cancellation: cancellation)
            try cancellation.check()
            outcome = .manifest(manifest)
          case "generate":
            let result = try await backend.generate(request, cancellation: cancellation) {
              offset, text in
              if !cancellation.isCancelled {
                sink.send(
                  id: request.id, event: "delta",
                  fields: ["replace_from_utf8": offset, "text": text])
              }
            }
            try cancellation.check()
            guard result.thinkingMode == request.thinking_mode else {
              throw WorkerError(
                "invalid_completion", "The backend did not confirm the requested thinking mode.")
            }
            outcome = .completion(result)
          case "unload":
            await backend.unload()
            outcome = .unloaded
          default: throw WorkerError.invalid
          }
        } catch let error as WorkerError {
          outcome = .error(cancellation.isCancelled ? .cancelled : error)
        } catch is CancellationError { outcome = .error(.cancelled) } catch {
          outcome = .error(
            WorkerError(
              "runtime_error", "The native model operation failed. No fallback was attempted."))
        }
        await self?.finished(id: request.id, outcome: outcome)
      }
      active = (request.id, cancellation, task)
      if hardDeadlines {
        // Loading/pre-fill may be inside a non-interruptible native operation. This process is the isolation boundary.
        Task.detached { [weak self] in
          try? await Task.sleep(for: .seconds(request.op == "generate" ? 120 : 180))
          await self?.timeout(id: request.id)
        }
      }
    }
    return true
  }
  private func finished(id: String, outcome: Outcome) {
    guard active?.id == id else { return }
    active = nil
    switch outcome {
    case .manifest(let manifest):
      if let data = try? JSONEncoder().encode(manifest),
        let object = try? JSONSerialization.jsonObject(with: data)
      {
        sink.send(
          id: id, event: "done",
          fields: ["manifest": object, "capabilities": manifest.capabilities])
      } else {
        sink.error(id: id, WorkerError("runtime_error", "Manifest serialization failed."))
      }
    case .completion(let result):
      var fields: [String: Any] = [
        "input_tokens": result.inputTokens, "output_tokens": result.outputTokens,
        "finish_reason": result.reason,
      ]
      if let mode = result.thinkingMode { fields["thinking_mode"] = mode.rawValue }
      sink.send(
        id: id, event: "done",
        fields: fields)
    case .unloaded: sink.send(id: id, event: "done")
    case .error(let error): sink.error(id: id, error)
    }
  }
  private func timeout(id: String) {
    guard let active, active.id == id else { return }
    active.cancel.cancel()
    active.task.cancel()
    scheduleDrainDeadline(id: id)
  }
  private func scheduleDrainDeadline(id: String) {
    guard hardDeadlines else { return }
    let deadline = drainDeadline
    Task.detached { [weak self] in
      try? await Task.sleep(for: deadline)
      await self?.terminateIfActive(id: id)
    }
  }
  private func terminateIfActive(id: String) { if active?.id == id { terminate(124) } }
  private func drain() async {
    if let active {
      active.cancel.cancel()
      active.task.cancel()
      scheduleDrainDeadline(id: active.id)
      await active.task.value
    }
    await backend.unload()
  }
  public func eof() async {
    closing = true
    await drain()
  }
}

public enum TextPatch {
  // UTF-8 offsets are always scalar boundaries, including when a tokenizer revises whitespace.
  public static func difference(previous: String, current: String) throws -> (Int, String) {
    let old = Array(previous.utf8)
    let new = Array(current.utf8)
    guard new.count <= Limits.outputBytes else {
      throw WorkerError("output_limit", "Generated text exceeded its byte budget.")
    }
    var common = zip(old, new).prefix(while: { $0 == $1 }).count
    while common > 0 && common < new.count && new[common] & 0xC0 == 0x80 { common -= 1 }
    let suffix = String(decoding: new[common...], as: UTF8.self)
    guard suffix.utf8.count <= 16_384 else {
      throw WorkerError("output_limit", "A decoded token exceeded the IPC text budget.")
    }
    return (common, suffix)
  }
}
