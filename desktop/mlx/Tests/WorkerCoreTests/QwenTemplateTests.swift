import Foundation
import XCTest

@testable import WorkerCore

final class QwenTemplateTests: XCTestCase {
  func template() throws -> SafeTemplate {
    let url = try XCTUnwrap(Bundle.module.url(forResource: "qwen3-template", withExtension: "jinja", subdirectory: "Fixtures"))
    let source = try String(contentsOf: url, encoding: .utf8)
    let template = try XCTUnwrap(SafeTemplate(source: source))
    XCTAssertEqual(template, .qwen3)
    XCTAssertNil(SafeTemplate(source: source + "\n"))
    return template
  }
  private func assertModes(
    _ template: SafeTemplate, _ messages: [Message], _ expected: String,
    file: StaticString = #filePath, line: UInt = #line
  ) throws {
    XCTAssertEqual(try template.render(messages), expected, file: file, line: line)
    XCTAssertEqual(
      try template.render(messages, thinkingMode: .modelDefault), expected,
      file: file, line: line)
    // This exact suffix is the pinned Jinja's enable_thinking=false branch;
    // every byte before it (including history processing) must stay unchanged.
    XCTAssertEqual(
      try template.render(messages, thinkingMode: .disabled),
      expected + "<think>\n\n</think>\n\n", file: file, line: line)
  }
  func testExactSourceAndOrdinaryTextConversation() throws {
    let template = try template()
    try assertModes(template, [
      .init(role: "system", content: "Be brief."),
      .init(role: "user", content: "Привет 🌍"),
    ], "<|im_start|>system\nBe brief.<|im_end|>\n<|im_start|>user\nПривет 🌍<|im_end|>\n<|im_start|>assistant\n")
    XCTAssertThrowsError(try template.render([.init(role: "tool", content: "not admitted")]))
    XCTAssertThrowsError(try template.render([]))
  }
  func testHistoricalReasoningIsRemovedButCurrentReasoningIsRetained() throws {
    let template = try template()
    try assertModes(template, [
      .init(role: "user", content: "First"),
      .init(role: "assistant", content: "<think>\nOld reasoning\n</think>\n\nOld answer"),
      .init(role: "user", content: "Follow up"),
      .init(role: "assistant", content: "<think>\nCurrent reasoning\n</think>\n\nCurrent answer"),
    ], "<|im_start|>user\nFirst<|im_end|>\n<|im_start|>assistant\nOld answer<|im_end|>\n<|im_start|>user\nFollow up<|im_end|>\n<|im_start|>assistant\n<think>\nCurrent reasoning\n</think>\n\nCurrent answer<|im_end|>\n<|im_start|>assistant\n")
  }
  func testExactDelimiterAndWrappedUserResponseSemantics() throws {
    let template = try template()
    try assertModes(template, [
      .init(role: "user", content: "Question"),
      .init(role: "assistant", content: "prefix<think>\nReason\n</think>discard</think>\nFinal"),
      .init(role: "user", content: "<tool_response>ordinary text</tool_response>"),
      .init(role: "assistant", content: "\nAnswer"),
    ], "<|im_start|>user\nQuestion<|im_end|>\n<|im_start|>assistant\n<think>\nReason\n</think>\n\nFinal<|im_end|>\n<|im_start|>user\n<tool_response>ordinary text</tool_response><|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\nAnswer<|im_end|>\n<|im_start|>assistant\n")
  }
  func testNewlineTrimmingUsesCodePointsRatherThanCRLFGraphemes() throws {
    let template = try template()
    try assertModes(template, [
      .init(role: "user", content: "Question"),
      .init(role: "assistant", content: "<think>\r\nR\r\n</think>\nA"),
    ], "<|im_start|>user\nQuestion<|im_end|>\n<|im_start|>assistant\n<think>\n\r\nR\r\n</think>\n\nA<|im_end|>\n<|im_start|>assistant\n")
  }
  func testTextCommandsNeverSelectModeAndOtherTemplatesRefuseDisabled() throws {
    try assertModes(try template(), [.init(role: "user", content: "/no_think /think")],
      "<|im_start|>user\n/no_think /think<|im_end|>\n<|im_start|>assistant\n")
    for source in [SafeTemplate.chatMLSource, SafeTemplate.graniteSource] {
      let other = try XCTUnwrap(SafeTemplate(source: source))
      let messages = [Message(role: "user", content: "ordinary")]
      XCTAssertEqual(try other.render(messages), try other.render(messages, thinkingMode: .modelDefault))
      XCTAssertThrowsError(try other.render(messages, thinkingMode: .disabled)) {
        XCTAssertEqual(($0 as? WorkerError)?.code, "unsupported_thinking_mode")
      }
    }
  }
}
