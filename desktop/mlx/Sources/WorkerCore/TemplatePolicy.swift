import CryptoKit
import Foundation

// These exact, bounded templates are interpreted directly. Arbitrary Jinja is never executed.
// Unsupported templates remain available for an explicitly requested raw-completion path.
public enum SafeTemplate: String, Sendable {
  case chatML = "simple-chatml-v1"
  case granite = "simple-granite-v1"
  case qwen3 = "qwen3-default-thinking-v1"
  // Exact published template, mlx-community/Qwen3-0.6B-4bit revision
  // 73e3e38d981303bc594367cd910ea6eb48349da8, tokenizer_config.json.
  // Only its no-tools, string-content branches are interpreted. An explicit
  // disabled mode selects this exact template's enable_thinking=false suffix.
  // The template itself is never evaluated as Jinja or executable code.
  static let qwen3SourceSHA256 =
    "87a2728cb8dc9fe424d624542f6060ec05a1d285ebbec578bb078900e33396b5"
  public static let chatMLSource =
    "{% for message in messages %}{{ '<|im_start|>' + message['role'] + '\\n' + message['content'] + '<|im_end|>\\n' }}{% endfor %}{% if add_generation_prompt %}{{ '<|im_start|>assistant\\n' }}{% endif %}"
  public static let graniteSource =
    "{% for message in messages %}{{ '<|start_of_role|>' + message['role'] + '<|end_of_role|>' + message['content'] + '<|end_of_text|>\\n' }}{% endfor %}{% if add_generation_prompt %}{{ '<|start_of_role|>assistant<|end_of_role|>' }}{% endif %}"
  public init?(source: String) {
    switch source {
    case Self.chatMLSource: self = .chatML
    case Self.graniteSource: self = .granite
    default:
      let digest = SHA256.hash(data: Data(source.utf8)).map { String(format: "%02x", $0) }.joined()
      guard digest == Self.qwen3SourceSHA256 else { return nil }
      self = .qwen3
    }
  }
  public func render(
    _ messages: [Message], thinkingMode: ThinkingMode = .modelDefault
  ) throws -> String {
    guard thinkingMode == .modelDefault || self == .qwen3 else {
      throw WorkerError(
        "unsupported_thinking_mode",
        "Disabled thinking requires the exact supported Qwen3 template.")
    }
    guard !messages.isEmpty, messages.count <= 64,
      messages.allSatisfy({ ["system", "user", "assistant"].contains($0.role) }),
      messages.reduce(0, { $0 + $1.content.utf8.count }) <= Limits.promptBytes
    else { throw WorkerError.invalid }
    switch self {
    case .chatML:
      return messages.map { "<|im_start|>\($0.role)\n\($0.content)<|im_end|>\n" }.joined()
        + "<|im_start|>assistant\n"
    case .granite:
      return messages.map {
        "<|start_of_role|>\($0.role)<|end_of_role|>\($0.content)<|end_of_text|>\n"
      }.joined() + "<|start_of_role|>assistant<|end_of_role|>"
    case .qwen3:
      return renderQwen3(messages, thinkingMode: thinkingMode)
    }
  }

  private func renderQwen3(_ messages: [Message], thinkingMode: ThinkingMode) -> String {
    // Match the selected template's last real user query. A literal wrapped
    // tool response remains ordinary user data; no tool capability is enabled.
    let lastQuery = messages.lastIndex {
      $0.role == "user"
        && !($0.content.hasPrefix("<tool_response>") && $0.content.hasSuffix("</tool_response>"))
    } ?? (messages.count - 1)
    func leftNewlines(_ value: String) -> String {
      String(decoding: value.utf8.drop(while: { $0 == 10 }), as: UTF8.self)
    }
    func rightNewlines(_ value: String) -> String {
      String(decoding: value.utf8.reversed().drop(while: { $0 == 10 }).reversed(), as: UTF8.self)
    }
    var output = ""
    for (index, message) in messages.enumerated() {
      var content = message.content
      if message.role == "assistant" {
        var reasoning = ""
        if content.contains("</think>") {
          let parts = content.components(separatedBy: "</think>")
          content = leftNewlines(parts.last!)
          reasoning = leftNewlines(
            rightNewlines(parts.first!).components(separatedBy: "<think>").last!)
        }
        if index > lastQuery && (index == messages.count - 1 || !reasoning.isEmpty) {
          content = "<think>\n" + leftNewlines(rightNewlines(reasoning))
            + "\n</think>\n\n" + leftNewlines(content)
        }
      }
      output += "<|im_start|>\(message.role)\n\(content)<|im_end|>\n"
    }
    // Only the generation suffix changes. History handling above remains the
    // pinned template's behavior in both modes; generated text is never filtered.
    return output + "<|im_start|>assistant\n"
      + (thinkingMode == .disabled ? "<think>\n\n</think>\n\n" : "")
  }
}
