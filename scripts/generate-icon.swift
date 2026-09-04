// Generate a 1024×1024 PNG icon for WireDesk: a monitor outline with a
// pointer arrow on it — "I'm driving someone else's screen" — on a dark
// rounded square. Run via `swift scripts/generate-icon.swift`; writes
// `assets/icon-source.png`, which feeds both the macOS AppIcon.icns
// (scripts/build-mac-app.sh) and the Windows .ico (scripts/icogen).
//
// The earlier icon was a white "W" on deep blue and was routinely mistaken
// for Microsoft Word — hence a pictogram rather than a letter, and a colour
// scheme nothing else on the desktop uses.

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

func rgb(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat) -> NSColor {
    NSColor(srgbRed: r / 255, green: g / 255, blue: b / 255, alpha: 1)
}

// Background plate. Inset from the full canvas so the icon matches the
// optical size of system icons in the Dock — a shape drawn edge-to-edge
// reads noticeably larger than its neighbours.
let inset: CGFloat = 82
let plateRect = NSRect(
    x: inset,
    y: inset,
    width: size.width - inset * 2,
    height: size.height - inset * 2
)
let plate = NSBezierPath(roundedRect: plateRect, xRadius: 196, yRadius: 196)
NSGradient(colors: [rgb(38, 48, 44), rgb(14, 20, 18)])!.draw(in: plate, angle: 270)

let accent = rgb(126, 231, 135)

// Monitor: rounded outline + neck + foot. Stroke weights are heavy on
// purpose — at 16 px the whole thing collapses to a silhouette, and a thin
// outline would disappear entirely.
let body = NSRect(x: 236, y: 330, width: 552, height: 400)
let bodyPath = NSBezierPath(roundedRect: body, xRadius: 56, yRadius: 56)
bodyPath.lineWidth = 62
accent.setStroke()
bodyPath.stroke()

accent.setFill()
NSBezierPath(
    roundedRect: NSRect(x: 452, y: 226, width: 120, height: 120),
    xRadius: 26,
    yRadius: 26
).fill()
NSBezierPath(
    roundedRect: NSRect(x: 340, y: 214, width: 344, height: 62),
    xRadius: 31,
    yRadius: 31
).fill()

// Pointer arrow sitting on the screen. Offsets are relative to the tip so
// the shape stays a classic cursor (tip, left edge, notch, tail) rather
// than the play-triangle a bare three-point path produces.
let tip = NSPoint(x: 424, y: 664)
let outline: [(CGFloat, CGFloat)] = [
    (0, 0),
    (0, -272),
    (78, -198),
    (126, -302),
    (186, -274),
    (138, -172),
    (212, -172),
]
let cursor = NSBezierPath()
for (i, d) in outline.enumerated() {
    let p = NSPoint(x: tip.x + d.0, y: tip.y + d.1)
    if i == 0 { cursor.move(to: p) } else { cursor.line(to: p) }
}
cursor.close()
NSColor.white.setFill()
cursor.fill()
// Stroke as well as fill: rounds the corners so the arrow doesn't look
// razor-sharp next to the rounded monitor, and thickens it for small sizes.
cursor.lineWidth = 34
cursor.lineJoinStyle = .round
NSColor.white.setStroke()
cursor.stroke()

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
