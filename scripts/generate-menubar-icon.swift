// Generate assets/menubar-icon.png — the macOS menu bar status item glyph.
// Run via `swift scripts/generate-menubar-icon.swift`.
//
// Same monitor-and-cursor pictogram as the app icon
// (scripts/generate-icon.swift), redrawn for a template image: no plate, no
// colour, just alpha. AppKit tints a template black on a light menu bar and
// white on a dark one, so any colour baked in here would be thrown away —
// and a scaled-down copy of the full app icon would show its dark plate as
// a solid blob.
//
// Written at 36 px for an 18 pt item: that is 1:1 on Retina, and the only
// size the status item needs (NSStatusItem scales the 2x bitmap down on
// non-Retina displays).
//
// Geometry is copied from generate-icon.swift's 1024-unit space and mapped
// onto the output canvas, so the two icons stay in sync by construction.

import AppKit
import CoreGraphics
import Foundation

let outSize: CGFloat = 36

// --- Source geometry, in the app icon's 1024×1024 space -----------------
let monitorBody = NSRect(x: 236, y: 330, width: 552, height: 400)
let monitorStroke: CGFloat = 62
let neck = NSRect(x: 452, y: 226, width: 120, height: 120)
let foot = NSRect(x: 340, y: 214, width: 344, height: 62)
let cursorTip = NSPoint(x: 424, y: 664)
let cursorOutline: [(CGFloat, CGFloat)] = [
    (0, 0), (0, -272), (78, -198), (126, -302), (186, -274), (138, -172), (212, -172),
]
let cursorStroke: CGFloat = 34

// Ink bounds: the monitor outline's outer edge, the foot, and nothing else
// (the cursor sits inside the screen).
let content = NSRect(
    x: monitorBody.minX - monitorStroke / 2,
    y: foot.minY,
    width: monitorBody.width + monitorStroke,
    height: monitorBody.maxY + monitorStroke / 2 - foot.minY
)

guard
    let ctx = CGContext(
        data: nil,
        width: Int(outSize),
        height: Int(outSize),
        bitsPerComponent: 8,
        bytesPerRow: 0,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    )
else { fatalError("cannot create CGContext") }

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)

// Fit the content box into the canvas, centred, leaving a hairline margin
// so the outline's antialiased edge isn't clipped.
let margin: CGFloat = 1
let scale = min(
    (outSize - margin * 2) / content.width,
    (outSize - margin * 2) / content.height
)
ctx.translateBy(
    x: (outSize - content.width * scale) / 2,
    y: (outSize - content.height * scale) / 2
)
ctx.scaleBy(x: scale, y: scale)
ctx.translateBy(x: -content.minX, y: -content.minY)

let ink = NSColor.black
ink.setFill()
ink.setStroke()

let bodyPath = NSBezierPath(roundedRect: monitorBody, xRadius: 56, yRadius: 56)
bodyPath.lineWidth = monitorStroke
bodyPath.stroke()
NSBezierPath(roundedRect: neck, xRadius: 26, yRadius: 26).fill()
NSBezierPath(roundedRect: foot, xRadius: 31, yRadius: 31).fill()

// The cursor is drawn a touch smaller than in the app icon and recentred in
// the hollow screen. At full size it ends one unit shy of the bottom
// bezel — invisible in colour, but in a single-colour silhouette the arrow
// would fuse with the frame and read as a smudge.
let screen = monitorBody.insetBy(dx: monitorStroke / 2, dy: monitorStroke / 2)
let cursor = NSBezierPath()
for (i, d) in cursorOutline.enumerated() {
    let p = NSPoint(x: cursorTip.x + d.0, y: cursorTip.y + d.1)
    if i == 0 { cursor.move(to: p) } else { cursor.line(to: p) }
}
cursor.close()
cursor.lineWidth = cursorStroke
cursor.lineJoinStyle = .round

let shrink: CGFloat = 0.82
// Bounds including the stroke, so the shrunk arrow really is centred.
let inked = cursor.bounds.insetBy(dx: -cursorStroke / 2, dy: -cursorStroke / 2)
var transform = AffineTransform(scaleByX: shrink, byY: shrink)
transform.append(
    AffineTransform(
        translationByX: screen.midX - inked.midX * shrink,
        byY: screen.midY - inked.midY * shrink
    )
)
cursor.transform(using: transform)
cursor.lineWidth = cursorStroke * shrink
cursor.fill()
cursor.stroke()

NSGraphicsContext.restoreGraphicsState()

guard let cgImage = ctx.makeImage() else { fatalError("cannot make CGImage") }
let rep = NSBitmapImageRep(cgImage: cgImage)
guard let pngData = rep.representation(using: .png, properties: [:]) else {
    fatalError("cannot encode PNG")
}

let scriptURL = URL(fileURLWithPath: CommandLine.arguments[0])
let repoRoot = scriptURL.deletingLastPathComponent().deletingLastPathComponent()
let outURL = repoRoot.appendingPathComponent("assets/menubar-icon.png")
try pngData.write(to: outURL)
print("wrote \(outURL.path) (\(pngData.count) bytes)")
