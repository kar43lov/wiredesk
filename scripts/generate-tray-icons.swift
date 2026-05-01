// Generate three 16×16 tray icons (green / yellow / gray "W") for the
// Windows host tray. Run via `swift scripts/generate-tray-icons.swift` —
// outputs assets/tray-{green,yellow,gray}.png.

import AppKit
import CoreGraphics
import Foundation

struct Variant {
    let name: String
    let color: NSColor
}

let variants: [Variant] = [
    .init(name: "green",  color: NSColor(srgbRed: 0.13, green: 0.70, blue: 0.27, alpha: 1.0)),
    .init(name: "yellow", color: NSColor(srgbRed: 0.96, green: 0.78, blue: 0.10, alpha: 1.0)),
    .init(name: "gray",   color: NSColor(srgbRed: 0.55, green: 0.55, blue: 0.55, alpha: 1.0)),
]

let size = CGSize(width: 16, height: 16)

let scriptURL = URL(fileURLWithPath: CommandLine.arguments[0])
let repoRoot = scriptURL.deletingLastPathComponent().deletingLastPathComponent()
let assetsDir = repoRoot.appendingPathComponent("assets")
try? FileManager.default.createDirectory(at: assetsDir, withIntermediateDirectories: true)

for v in variants {
    guard
        let ctx = CGContext(
            data: nil,
            width: Int(size.width),
            height: Int(size.height),
            bitsPerComponent: 8,
            bytesPerRow: 0,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        )
    else { fatalError("cannot create CGContext for \(v.name)") }

    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)

    // Solid background of variant color.
    v.color.setFill()
    NSBezierPath(rect: NSRect(origin: .zero, size: size)).fill()

    // White "W" — bold, fits 16×16 tightly.
    let attrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 13, weight: .black),
        .foregroundColor: NSColor.white,
    ]
    let text = "W" as NSString
    let textSize = text.size(withAttributes: attrs)
    let origin = NSPoint(
        x: (size.width - textSize.width) / 2,
        y: (size.height - textSize.height) / 2 - 1
    )
    text.draw(at: origin, withAttributes: attrs)

    NSGraphicsContext.restoreGraphicsState()

    guard let cgImage = ctx.makeImage() else { fatalError("cannot makeImage for \(v.name)") }
    let rep = NSBitmapImageRep(cgImage: cgImage)
    guard let png = rep.representation(using: .png, properties: [:]) else {
        fatalError("cannot encode PNG for \(v.name)")
    }
    let out = assetsDir.appendingPathComponent("tray-\(v.name).png")
    try png.write(to: out)
    print("wrote \(out.path) (\(png.count) bytes)")
}
