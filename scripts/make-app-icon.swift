// Draws Requiem's app icon: writes <out dir>/AppIcon.iconset and <out dir>/AppIcon.icns.
// Usage: swift scripts/make-app-icon.swift <out dir>
// The committed icon is crates/requiem-gui/assets/AppIcon.icns.
import AppKit

let args = CommandLine.arguments
guard args.count == 2 else {
    FileHandle.standardError.write("usage: swift make-app-icon.swift <out dir>\n".data(using: .utf8)!)
    exit(2)
}
let outDir = URL(fileURLWithPath: args[1], isDirectory: true)
let srgb = CGColorSpace(name: CGColorSpace.sRGB)!

func rgb(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    let c = { (shift: UInt32) in CGFloat((hex >> shift) & 0xff) / 255 }
    return CGColor(colorSpace: srgb, components: [c(16), c(8), c(0), alpha])!
}

let bgTop = rgb(0x1A1A20)
let bgBottom = rgb(0x0A0A0C)
let accent = rgb(0x7C70F5)
let pink = rgb(0xF0A8D0)

// The macOS app-icon grid on a 1024 canvas: an 824 square with continuous corners.
let shapeRect = CGRect(x: 100, y: 100, width: 824, height: 824)
let cornerRadius: CGFloat = 185.4

// Continuous corners, like Apple's icon shape: the curvature eases in over 1.52866 radii
// instead of jumping where a circular arc meets the edge. A corner's points are in units of
// the radius, back along the edge it arrives on and forward along the edge it leaves on.
func continuousRoundedRect(_ rect: CGRect, radius r: CGFloat) -> CGPath {
    let path = CGMutablePath()
    // Counter-clockwise from bottom-right.
    let corners: [(CGPoint, CGVector, CGVector)] = [
        (CGPoint(x: rect.maxX, y: rect.minY), CGVector(dx: -1, dy: 0), CGVector(dx: 0, dy: 1)),
        (CGPoint(x: rect.maxX, y: rect.maxY), CGVector(dx: 0, dy: -1), CGVector(dx: -1, dy: 0)),
        (CGPoint(x: rect.minX, y: rect.maxY), CGVector(dx: 1, dy: 0), CGVector(dx: 0, dy: -1)),
        (CGPoint(x: rect.minX, y: rect.minY), CGVector(dx: 0, dy: 1), CGVector(dx: 1, dy: 0)),
    ]
    for (i, (c, back, fwd)) in corners.enumerated() {
        func p(_ a: CGFloat, _ b: CGFloat) -> CGPoint {
            CGPoint(x: c.x + (a * back.dx + b * fwd.dx) * r, y: c.y + (a * back.dy + b * fwd.dy) * r)
        }
        if i == 0 { path.move(to: p(1.52866483, 0)) } else { path.addLine(to: p(1.52866483, 0)) }
        path.addCurve(to: p(0.66993427, 0.06549600), control1: p(1.08849323, 0), control2: p(0.86840689, 0))
        path.addLine(to: p(0.63149399, 0.07491100))
        path.addCurve(to: p(0.07491100, 0.63149399), control1: p(0.37282392, 0.16905899), control2: p(0.16905899, 0.37282392))
        path.addLine(to: p(0.06549600, 0.66993427))
        path.addCurve(to: p(0, 1.52866483), control1: p(0, 0.86840689), control2: p(0, 1.08849323))
    }
    path.closeSubpath()
    return path
}

// Equalizer bars, left to right: heights as a fraction of the tallest, centred on the
// shape's middle.
let barHeights: [CGFloat] = [0.46, 0.80, 1.0, 0.62]
let barWidth: CGFloat = 92
let barGap: CGFloat = 64
let tallestBar: CGFloat = 470

func draw(_ ctx: CGContext, pixels: Int) {
    let s = CGFloat(pixels) / 1024
    ctx.scaleBy(x: s, y: s)
    let shape = continuousRoundedRect(shapeRect, radius: cornerRadius)

    // Shadow offsets and blur are in device space, so they scale by hand.
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -12 * s), blur: 28 * s, color: rgb(0x000000, 0.45))
    ctx.addPath(shape)
    ctx.setFillColor(bgBottom)
    ctx.fillPath()
    ctx.restoreGState()

    ctx.saveGState()
    ctx.addPath(shape)
    ctx.clip()
    let fill = CGGradient(colorsSpace: srgb, colors: [bgTop, bgBottom] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(fill, start: CGPoint(x: 0, y: shapeRect.maxY), end: CGPoint(x: 0, y: shapeRect.minY), options: [])
    let haze = CGGradient(colorsSpace: srgb, colors: [rgb(0x7C70F5, 0.16), rgb(0x7C70F5, 0)] as CFArray, locations: [0, 1])!
    ctx.drawRadialGradient(haze, startCenter: CGPoint(x: 512, y: 470), startRadius: 0, endCenter: CGPoint(x: 512, y: 470), endRadius: 420, options: [])

    // A faint highlight along the top edge, fading out before the corners turn down.
    ctx.saveGState()
    ctx.addPath(shape)
    ctx.setLineWidth(6)
    ctx.replacePathWithStrokedPath()
    ctx.clip()
    let rim = CGGradient(colorsSpace: srgb, colors: [rgb(0xFFFFFF, 0.22), rgb(0xFFFFFF, 0)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(rim, start: CGPoint(x: 0, y: shapeRect.maxY), end: CGPoint(x: 0, y: shapeRect.maxY - 260), options: [])
    ctx.restoreGState()

    let bars = CGMutablePath()
    let total = CGFloat(barHeights.count) * barWidth + CGFloat(barHeights.count - 1) * barGap
    var x = shapeRect.midX - total / 2
    for h in barHeights {
        let height = h * tallestBar
        let bar = CGRect(x: x, y: shapeRect.midY - height / 2, width: barWidth, height: height)
        bars.addRoundedRect(in: bar, cornerWidth: barWidth / 2, cornerHeight: barWidth / 2)
        x += barWidth + barGap
    }
    let glyph = bars.boundingBox
    let ink = CGGradient(colorsSpace: srgb, colors: [accent, pink] as CFArray, locations: [0, 1])!

    // A soft accent glow under the bars.
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: 60 * s, color: rgb(0x7C70F5, 0.55))
    ctx.addPath(bars)
    ctx.setFillColor(rgb(0x7C70F5, 0.6))
    ctx.fillPath()
    ctx.restoreGState()

    ctx.saveGState()
    ctx.addPath(bars)
    ctx.clip()
    ctx.drawLinearGradient(ink, start: CGPoint(x: glyph.minX, y: glyph.minY), end: CGPoint(x: glyph.maxX, y: glyph.maxY), options: [])
    ctx.restoreGState()

    ctx.restoreGState()
}

func render(pixels: Int) -> CGImage {
    let ctx = CGContext(data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0,
                        space: srgb, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    draw(ctx, pixels: pixels)
    return ctx.makeImage()!
}

func writePNG(_ image: CGImage, to url: URL) {
    guard let dest = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil) else {
        fatalError("cannot write \(url.path)")
    }
    CGImageDestinationAddImage(dest, image, nil)
    guard CGImageDestinationFinalize(dest) else { fatalError("cannot write \(url.path)") }
}

let iconset = outDir.appendingPathComponent("AppIcon.iconset", isDirectory: true)
try? FileManager.default.removeItem(at: iconset)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for points in [16, 32, 128, 256, 512] {
    writePNG(render(pixels: points), to: iconset.appendingPathComponent("icon_\(points)x\(points).png"))
    writePNG(render(pixels: points * 2), to: iconset.appendingPathComponent("icon_\(points)x\(points)@2x.png"))
}

let icns = outDir.appendingPathComponent("AppIcon.icns")
let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["-c", "icns", iconset.path, "-o", icns.path]
try iconutil.run()
iconutil.waitUntilExit()
guard iconutil.terminationStatus == 0 else {
    FileHandle.standardError.write("make-app-icon.swift: iconutil failed\n".data(using: .utf8)!)
    exit(1)
}
print(icns.path)
