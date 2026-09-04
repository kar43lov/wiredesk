// Generate three 16×16 tray icons (green / yellow / gray) for the Windows
// host tray. Run via `swift scripts/generate-tray-icons.swift` — outputs
// assets/tray-{green,yellow,gray}.png.
//
// The glyph is the same monitor silhouette as the app icon
// (scripts/generate-icon.swift), knocked out of a solid status-coloured
// square. It used to be a white "W", which read as Microsoft Word.
// Deliberately *not* the full app icon scaled down: at 16 px the cursor
// arrow and the monitor outline turn to mush, so the tray gets a simplified
// filled screen instead. Colour, not shape, carries the state here.

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

    // White monitor silhouette: screen body, neck, foot. Coordinates are
    // whole pixels — a half-pixel edge at this size renders as grey mush.
    NSColor.white.setFill()
    NSBezierPath(roundedRect: NSRect(x: 2, y: 6, width: 12, height: 8),
                 xRadius: 1.5, yRadius: 1.5).fill()
    NSBezierPath(rect: NSRect(x: 7, y: 4, width: 2, height: 2)).fill()
    NSBezierPath(rect: NSRect(x: 4, y: 3, width: 8, height: 1)).fill()
    // Punch the screen out so the shape doesn't read as a solid blob.
    v.color.setFill()
    NSBezierPath(rect: NSRect(x: 4, y: 8, width: 8, height: 4)).fill()

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
