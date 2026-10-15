import Darwin
import Foundation

public enum Limits {
  public static let frameBytes = 131_072
  public static let promptBytes = 65_536
  public static let outputBytes = 524_288
  public static let inputTokens = 8_192
  public static let outputTokens = 2_048
  public static let files = 128
  public static let weightsBytes: Int64 = 16 * 1_024 * 1_024 * 1_024
  public static let jsonBytes = 32 * 1_024 * 1_024
  public static let requests = 4_096
}

public struct WorkerError: Error, Sendable, Equatable {
  public let code: String
  public let message: String
  public init(_ code: String, _ message: String) {
    self.code = code
    self.message = message
  }
  public static let invalid = Self("invalid_request", "The request does not match protocol v1.")
  public static let cancelled = Self("cancelled", "The operation was cancelled.")
}

public struct Message: Codable, Sendable, Equatable {
  public let role: String
  public let content: String
  public init(role: String, content: String) {
    self.role = role
    self.content = content
  }
}

public enum ThinkingMode: String, Codable, Sendable, Equatable {
  case modelDefault = "model_default"
  case disabled
}

public struct Request: Decodable, Sendable {
  public let v: Int
  public let id: String
  public let op: String
  public let model_root: String?
  public let target_id: String?
  public let expected_package_sha256: String?
  public let mode: String?
  public let prompt: String?
  public let messages: [Message]?
  public let max_tokens: Int?
  public let temperature: Float?
  public let top_p: Float?
  public let seed: UInt64?
  public let thinking_mode: ThinkingMode?

  public static func decode(_ bytes: Data) throws -> Self {
    guard bytes.count <= Limits.frameBytes else {
      throw WorkerError("frame_too_large", "IPC frame exceeds its byte limit.")
    }
    let object = try BoundedJSON.object(bytes, maxDepth: 12)
    guard let op = object["op"] as? String else { throw WorkerError.invalid }
    var keys: Set<String> = ["v", "id", "op"]
    switch op {
    case "hello", "unload", "shutdown": break
    case "inspect", "load": keys.formUnion(["model_root", "expected_package_sha256"])
    case "cancel": keys.insert("target_id")
    case "generate":
      keys.formUnion([
        "mode", "prompt", "messages", "max_tokens", "temperature", "top_p", "seed",
        "thinking_mode",
      ])
    default:
      throw WorkerError("unsupported_operation", "This worker supports local text generation only.")
    }
    guard Set(object.keys).isSubset(of: keys) else { throw WorkerError.invalid }
    let value: Self
    do { value = try JSONDecoder().decode(Self.self, from: bytes) } catch {
      throw WorkerError.invalid
    }
    guard value.v == 1, validID(value.id) else { throw WorkerError.invalid }
    switch op {
    case "inspect", "load":
      guard let path = value.model_root, path.utf8.count <= 4096, path.hasPrefix("/"),
        !path.contains("\0")
      else { throw WorkerError.invalid }
      if op == "load" && value.expected_package_sha256 == nil { throw WorkerError.invalid }
      if let hash = value.expected_package_sha256 {
        guard hash.utf8.count == 64,
          hash.utf8.allSatisfy({ (48...57).contains($0) || (97...102).contains($0) })
        else { throw WorkerError.invalid }
      }
    case "cancel": guard let id = value.target_id, validID(id) else { throw WorkerError.invalid }
    case "generate":
      // Missing means the original default. Explicit null must not erase user intent.
      if object.keys.contains("thinking_mode") && value.thinking_mode == nil {
        throw WorkerError.invalid
      }
      guard let max = value.max_tokens, (1...Limits.outputTokens).contains(max),
        let temperature = value.temperature, temperature.isFinite, (0...2).contains(temperature),
        let topP = value.top_p, topP.isFinite, topP > 0, topP <= 1
      else { throw WorkerError.invalid }
      if value.mode == "raw" {
        guard value.thinking_mode == nil else {
          throw WorkerError(
            "unsupported_thinking_mode", "Thinking mode applies only to explicitly templated chat.")
        }
        guard let prompt = value.prompt, !prompt.isEmpty, prompt.utf8.count <= Limits.promptBytes,
          value.messages == nil
        else { throw WorkerError.invalid }
      } else if value.mode == "chat" {
        guard value.prompt == nil, let messages = value.messages, !messages.isEmpty,
          messages.count <= 64
        else { throw WorkerError.invalid }
        guard let raw = object["messages"] as? [[String: Any]],
          raw.allSatisfy({ Set($0.keys) == ["role", "content"] }),
          messages.allSatisfy({ ["system", "user", "assistant"].contains($0.role) }),
          messages.reduce(0, { $0 + $1.content.utf8.count }) <= Limits.promptBytes
        else { throw WorkerError.invalid }
      } else {
        throw WorkerError(
          "unsupported_capability", "Generation mode must be explicitly raw or chat.")
      }
    default: break
    }
    return value
  }
  public static func validID(_ id: String) -> Bool {
    !id.isEmpty && id.utf8.count <= 64
      && id.utf8.allSatisfy {
        (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45
          || $0 == 95
      }
  }
}

// Reject duplicate object keys and excessive nesting before Foundation allocates a JSON tree.
public enum BoundedJSON {
  public static func object(_ data: Data, maxDepth: Int = 32) throws -> [String: Any] {
    guard data.count <= Limits.jsonBytes, String(data: data, encoding: .utf8) != nil else {
      throw WorkerError("invalid_json", "JSON is too large or is not UTF-8.")
    }
    var scanner = JSONGuard(bytes: Array(data), maxDepth: maxDepth)
    try scanner.value(depth: 0)
    scanner.space()
    guard scanner.i == scanner.bytes.count,
      let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
    else { throw WorkerError("invalid_json", "JSON must be a valid object.") }
    return object
  }
}
private struct JSONGuard {
  let bytes: [UInt8]
  let maxDepth: Int
  var i = 0
  var nodes = 0
  mutating func space() { while i < bytes.count && [9, 10, 13, 32].contains(bytes[i]) { i += 1 } }
  mutating func string() throws -> String {
    let begin = i
    guard i < bytes.count, bytes[i] == 34 else { throw WorkerError.invalid }
    i += 1
    while i < bytes.count {
      let b = bytes[i]
      i += 1
      if b == 34 {
        let data = Data(bytes[begin..<i])
        guard let result = try? JSONDecoder().decode(String.self, from: data) else {
          throw WorkerError.invalid
        }
        return result
      }
      if b == 92 {
        guard i < bytes.count else { throw WorkerError.invalid }
        i += 1
      } else if b < 32 {
        throw WorkerError.invalid
      }
    }
    throw WorkerError.invalid
  }
  mutating func value(depth: Int) throws {
    nodes += 1
    guard depth <= maxDepth, nodes <= 2_000_000 else {
      throw WorkerError("resource_limit", "JSON structure exceeds its limit.")
    }
    space()
    guard i < bytes.count else { throw WorkerError.invalid }
    if bytes[i] == 34 {
      _ = try string()
      return
    }
    if bytes[i] == 123 || bytes[i] == 91 {
      let isObject = bytes[i] == 123
      let end: UInt8 = isObject ? 125 : 93
      i += 1
      space()
      if i < bytes.count && bytes[i] == end {
        i += 1
        return
      }
      var keys = Set<String>()
      while i < bytes.count {
        if isObject {
          let key = try string()
          guard keys.insert(key).inserted else {
            throw WorkerError("duplicate_json_key", "Duplicate JSON keys are not accepted.")
          }
          space()
          guard i < bytes.count && bytes[i] == 58 else { throw WorkerError.invalid }
          i += 1
        }
        try value(depth: depth + 1)
        space()
        guard i < bytes.count else { throw WorkerError.invalid }
        let separator = bytes[i]
        i += 1
        if separator == end { return }
        guard separator == 44 else { throw WorkerError.invalid }
        space()
      }
      throw WorkerError.invalid
    }
    let begin = i
    while i < bytes.count && ![9, 10, 13, 32, 44, 93, 125].contains(bytes[i]) { i += 1 }
    guard i > begin else { throw WorkerError.invalid }
  }
}

public final class FrameReader {
  private var buffered = Data()
  private let fd: Int32
  public init(fd: Int32 = STDIN_FILENO) { self.fd = fd }
  public func next() throws -> Data? {
    while true {
      if let end = buffered.firstIndex(of: 10) {
        let frame = Data(buffered[..<end])
        buffered.removeSubrange(...end)
        guard frame.count <= Limits.frameBytes else {
          throw WorkerError("frame_too_large", "IPC frame exceeds its byte limit.")
        }
        return frame
      }
      guard buffered.count <= Limits.frameBytes else {
        throw WorkerError("frame_too_large", "IPC frame exceeds its byte limit.")
      }
      var chunk = [UInt8](repeating: 0, count: 4096)
      let n = Darwin.read(fd, &chunk, chunk.count)
      if n < 0 {
        if errno == EINTR { continue }
        throw WorkerError("ipc_io", "IPC input failed.")
      }
      if n == 0 {
        guard buffered.isEmpty else {
          throw WorkerError("truncated_frame", "EOF interrupted an IPC frame.")
        }
        return nil
      }
      buffered.append(contentsOf: chunk[..<n])
    }
  }
}

public final class EventSink: @unchecked Sendable {
  private let lock = NSLock()
  private let fd: Int32
  public init(fd: Int32) {
    self.fd = fd
    _ = fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_NONBLOCK)
  }
  public func send(id: String?, event: String, fields: [String: Any] = [:]) {
    lock.lock()
    defer { lock.unlock() }
    var object = fields
    object["v"] = 1
    object["id"] = id ?? NSNull()
    object["event"] = event
    guard
      var data = try? JSONSerialization.data(
        withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes]),
      data.count <= Limits.frameBytes
    else { _exit(74) }
    data.append(10)
    let deadline = ProcessInfo.processInfo.systemUptime + 5
    data.withUnsafeBytes { raw in
      var position = 0
      while position < data.count {
        var descriptor = pollfd(fd: fd, events: Int16(POLLOUT), revents: 0)
        guard ProcessInfo.processInfo.systemUptime < deadline else { _exit(74) }
        let ready = poll(&descriptor, 1, 100)
        if ready < 0 && errno == EINTR { continue }
        guard ready >= 0, descriptor.revents & Int16(POLLERR | POLLHUP | POLLNVAL) == 0 else {
          _exit(74)
        }
        if ready == 0 { continue }
        let written = Darwin.write(
          fd, raw.baseAddress!.advanced(by: position), data.count - position)
        if written < 0 && [EINTR, EAGAIN].contains(errno) { continue }
        guard written > 0 else { _exit(74) }
        position += written
      }
    }
  }
  public func error(id: String?, _ error: WorkerError) {
    send(id: id, event: "error", fields: ["code": error.code, "message": error.message])
  }
}
