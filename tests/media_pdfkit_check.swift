// Independent Apple PDFKit check of the committed synthetic Unicode fixture.
// swift -module-cache-path target/swift-module-cache tests/media_pdfkit_check.swift
import Foundation
import PDFKit

let fixture = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "crates/smith-media/tests/fixtures/unicode.pdf"
guard let document = PDFDocument(url: URL(fileURLWithPath: fixture)),
      document.pageCount == 1,
      let text = document.string,
      text.contains("Привет, мир! Ёж"),
      text.contains("00123"),
      text.contains("9007199254740993.00") else {
    fatalError("Synthetic PDF Unicode extraction failed")
}
print("PDFKit PASS: one page; Cyrillic, leading zeros and exact numeric text")
