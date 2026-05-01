// Generate a 1024×1024 PNG icon for WireDesk: white "W" on a deep-blue
// rounded-square background. Run via `swift scripts/generate-icon.swift`
// — outputs `assets/icon-source.png` next to the script's repo root.

import AppKit
import CoreGraphics
import Foundation

let size = CGSize(width: 1024, height: 1024)

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
else { fatalError("cannot create CGContext") }

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)

// Background — deep blue with subtle gradient.
let bg = NSBezierPath(
    roundedRect: NSRect(origin: .zero, size: size),
    xRadius: 180,
    yRadius: 180
)
let top = NSColor(srgbRed: 0.10, green: 0.20, blue: 0.55, alpha: 1.0)
let bot = NSColor(srgbRed: 0.05, green: 0.10, blue: 0.30, alpha: 1.0)
let grad = NSGradient(colors: [top, bot])!
grad.draw(in: bg, angle: 270)

// White "W" centered.
let attrs: [NSAttributedString.Key: Any] = [
    .font: NSFont.systemFont(ofSize: 720, weight: .heavy),
    .foregroundColor: NSColor.white,
]
let text = "W" as NSString
let textSize = text.size(withAttributes: attrs)
let origin = NSPoint(
    x: (size.width - textSize.width) / 2,
    y: (size.height - textSize.height) / 2 - 30
)
text.draw(at: origin, withAttributes: attrs)

NSGraphicsContext.restoreGraphicsState()

guard let cgImage = ctx.makeImage() else { fatalError("cannot make CGImage") }

let rep = NSBitmapImageRep(cgImage: cgImage)
guard let pngData = rep.representation(using: .png, properties: [:]) else {
    fatalError("cannot encode PNG")
}

let scriptURL = URL(fileURLWithPath: CommandLine.arguments[0])
let repoRoot = scriptURL.deletingLastPathComponent().deletingLastPathComponent()
let outURL = repoRoot.appendingPathComponent("assets/icon-source.png")

try? FileManager.default.createDirectory(
    at: outURL.deletingLastPathComponent(),
    withIntermediateDirectories: true
)
try pngData.write(to: outURL)
print("wrote \(outURL.path) (\(pngData.count) bytes)")
