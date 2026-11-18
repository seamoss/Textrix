// Textrix.app's icon (textrix app install): a dark terminal tile, a neon
// prompt, an irssi-style status bar. To draw it again:
//   swift packaging/macos/icon.swift /tmp/icon-1024.png
// then make Textrix.icns from that with sips and iconutil (an .iconset of
// 16 to 512 points, each at 1x and 2x).
import AppKit

let size: CGFloat = 1024
let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()
let ctx = NSGraphicsContext.current!.cgContext

// macOS icon grid: the tile inset from the canvas, with rounded corners.
let inset: CGFloat = 100
let tile = NSRect(x: inset, y: inset, width: size - 2 * inset, height: size - 2 * inset)
let path = NSBezierPath(roundedRect: tile, xRadius: 185, yRadius: 185)
ctx.saveGState()
path.addClip()
NSGradient(starting: NSColor(red: 0.16, green: 0.06, blue: 0.27, alpha: 1),
           ending: NSColor(red: 0.04, green: 0.03, blue: 0.10, alpha: 1))!.draw(in: tile, angle: -90)

// The status bar along the bottom, and the window list's edge.
NSColor(red: 0.0, green: 0.37, blue: 0.53, alpha: 1).setFill()
NSRect(x: tile.minX, y: tile.minY + 120, width: tile.width, height: 70).fill()
NSColor(white: 1, alpha: 0.16).setFill()
NSRect(x: tile.minX + 210, y: tile.minY + 190, width: 6, height: tile.height - 190).fill()

func draw(_ text: String, size: CGFloat, color: NSColor, at point: NSPoint, glow: Bool) {
    let font = NSFont.monospacedSystemFont(ofSize: size, weight: .bold)
    let shadow = NSShadow()
    shadow.shadowColor = glow ? color.withAlphaComponent(0.85) : .clear
    shadow.shadowBlurRadius = glow ? 28 : 0
    let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color, .shadow: shadow]
    (text as NSString).draw(at: point, withAttributes: attrs)
}
let mint = NSColor(red: 0.45, green: 0.95, blue: 0.72, alpha: 1)
let pink = NSColor(red: 1.0, green: 0.38, blue: 0.77, alpha: 1)
// Window numbers down the side, the prompt in the middle.
for (n, y) in [(1, 690.0), (2, 600.0), (3, 510.0)] {
    draw("\(n)", size: 64, color: n == 2 ? pink : NSColor(white: 1, alpha: 0.45), at: NSPoint(x: tile.minX + 70, y: y), glow: n == 2)
}
draw(">_", size: 330, color: mint, at: NSPoint(x: tile.minX + 265, y: 360), glow: true)
draw("[textrix]", size: 50, color: NSColor(white: 0.92, alpha: 1), at: NSPoint(x: tile.minX + 40, y: tile.minY + 128), glow: false)
ctx.restoreGState()
image.unlockFocus()

let rep = NSBitmapImageRep(data: image.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
