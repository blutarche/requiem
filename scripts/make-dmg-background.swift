// Draws the installer window's background: writes background.png (660×420), background@2x.png
// and background.tiff (both, for tiffutil's HiDPI pairing) into <out dir>.
// Usage: swift scripts/make-dmg-background.swift <out dir>
// The icon positions and window size must match scripts/dmg-settings.py.
import AppKit

let args = CommandLine.arguments
guard args.count == 2 else {
    FileHandle.standardError.write("usage: swift make-dmg-background.swift <out dir>\n".data(using: .utf8)!)
    exit(2)
}
let outDir = URL(fileURLWithPath: args[1], isDirectory: true)
let srgb = CGColorSpace(name: CGColorSpace.sRGB)!

func rgb(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    let c = { (shift: UInt32) in CGFloat((hex >> shift) & 0xff) / 255 }
    return CGColor(colorSpace: srgb, components: [c(16), c(8), c(0), alpha])!
}

let width = 660, height = 420
let paper: UInt32 = 0xFAFAFC
let accent: UInt32 = 0x7C70F5
let accentDeep: UInt32 = 0x5A4FC0
let pink: UInt32 = 0xF0A8D0
let gold: UInt32 = 0xE0B85D
let arrowGrey: UInt32 = 0xC9C9D1

// Finder's coordinates: points from the top-left, icon centres.
let iconCentres = [CGPoint(x: 180, y: 205), CGPoint(x: 480, y: 205)]
let clearRadius: CGFloat = 90
// Where Finder writes the names under the 128 pt icons.
let labelRows: ClosedRange<CGFloat> = 262...312

// Bars standing on `base`, left edge at `x`, with round ends.
func equalizer(_ ctx: CGContext, x: CGFloat, base: CGFloat, heights: [CGFloat], bar: CGFloat, gap: CGFloat, colors: [UInt32]) {
    let path = CGMutablePath()
    for (i, h) in heights.enumerated() {
        let r = CGRect(x: x + CGFloat(i) * (bar + gap), y: base - h, width: bar, height: h)
        path.addRoundedRect(in: r, cornerWidth: bar / 2, cornerHeight: bar / 2)
    }
    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    let box = path.boundingBox
    let ink = CGGradient(colorsSpace: srgb, colors: colors.map { rgb($0) } as CFArray, locations: nil)!
    ctx.drawLinearGradient(ink, start: CGPoint(x: box.minX, y: box.maxY), end: CGPoint(x: box.maxX, y: box.minY), options: [])
    ctx.restoreGState()
}

// An eighth note `size` tall, (0, 0) at the top of its stem: a tilted head, the stem and a
// curled flag.
func note(_ ctx: CGContext, at p: CGPoint, size: CGFloat, tilt: CGFloat, color: UInt32) {
    let u = size / 24
    func q(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: x * u, y: y * u) }
    ctx.saveGState()
    ctx.translateBy(x: p.x, y: p.y)
    ctx.rotate(by: tilt * .pi / 180)
    ctx.setFillColor(rgb(color))
    ctx.saveGState()
    ctx.translateBy(x: 0, y: 19.5 * u)
    ctx.rotate(by: -22 * .pi / 180)
    ctx.fillEllipse(in: CGRect(x: -5.6 * u, y: -4.2 * u, width: 11.2 * u, height: 8.4 * u))
    ctx.restoreGState()
    ctx.addPath(CGPath(roundedRect: CGRect(x: 3.4 * u, y: 0, width: 2.2 * u, height: 19 * u),
                       cornerWidth: 1.1 * u, cornerHeight: 1.1 * u, transform: nil))
    ctx.fillPath()
    ctx.move(to: q(3.4, 0.8))
    ctx.addCurve(to: q(10.6, 11.8), control1: q(5.6, 5.6), control2: q(12.6, 6.2))
    ctx.addCurve(to: q(8.6, 15.6), control1: q(10.4, 13.4), control2: q(9.6, 14.8))
    ctx.addCurve(to: q(5.6, 7.4), control1: q(10.6, 11), control2: q(7.8, 8.6))
    ctx.closePath()
    ctx.fillPath()
    ctx.restoreGState()
}

func dot(_ ctx: CGContext, at p: CGPoint, radius: CGFloat, color: UInt32, alpha: CGFloat = 1) {
    ctx.setFillColor(rgb(color, alpha))
    ctx.fillEllipse(in: CGRect(x: p.x - radius, y: p.y - radius, width: radius * 2, height: radius * 2))
}

func ring(_ ctx: CGContext, at p: CGPoint, radius: CGFloat, line: CGFloat, color: UInt32) {
    ctx.setStrokeColor(rgb(color))
    ctx.setLineWidth(line)
    ctx.strokeEllipse(in: CGRect(x: p.x - radius, y: p.y - radius, width: radius * 2, height: radius * 2))
}

func draw(_ ctx: CGContext) {
    ctx.setFillColor(rgb(paper))
    ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))

    // Soft circles bleeding off the corners.
    dot(ctx, at: CGPoint(x: 628, y: 26), radius: 54, color: pink, alpha: 0.22)
    dot(ctx, at: CGPoint(x: 18, y: 404), radius: 62, color: accent, alpha: 0.12)
    dot(ctx, at: CGPoint(x: 560, y: 432), radius: 34, color: gold, alpha: 0.16)

    equalizer(ctx, x: 38, base: 78, heights: [12, 24, 32, 18], bar: 6, gap: 4.5, colors: [accent, pink])
    equalizer(ctx, x: 596, base: 384, heights: [20, 30, 14], bar: 6, gap: 4.5, colors: [accentDeep, accent])

    note(ctx, at: CGPoint(x: 268, y: 40), size: 26, tilt: -12, color: accentDeep)
    note(ctx, at: CGPoint(x: 86, y: 334), size: 22, tilt: 10, color: pink)
    note(ctx, at: CGPoint(x: 612, y: 196), size: 20, tilt: 14, color: gold)

    dot(ctx, at: CGPoint(x: 118, y: 104), radius: 4, color: gold)
    dot(ctx, at: CGPoint(x: 404, y: 58), radius: 3, color: pink)
    dot(ctx, at: CGPoint(x: 566, y: 106), radius: 3.5, color: accent)
    dot(ctx, at: CGPoint(x: 330, y: 372), radius: 3, color: accentDeep)
    dot(ctx, at: CGPoint(x: 24, y: 214), radius: 3.5, color: pink)
    ring(ctx, at: CGPoint(x: 452, y: 360), radius: 6, line: 2, color: gold)
    ring(ctx, at: CGPoint(x: 200, y: 386), radius: 5, line: 2, color: accent)

    // From the app towards Applications.
    let start = CGPoint(x: 284, y: 204), control = CGPoint(x: 330, y: 168), tip = CGPoint(x: 376, y: 204)
    ctx.setStrokeColor(rgb(arrowGrey))
    ctx.setLineWidth(3)
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.move(to: start)
    ctx.addQuadCurve(to: tip, control: control)
    let back = atan2(control.y - tip.y, control.x - tip.x)
    for spread in [-0.5, 0.5] {
        ctx.move(to: tip)
        ctx.addLine(to: CGPoint(x: tip.x + 11 * cos(back + spread), y: tip.y + 11 * sin(back + spread)))
    }
    ctx.strokePath()
}

func render(scale: Int) -> CGImage {
    let ctx = CGContext(data: nil, width: width * scale, height: height * scale, bitsPerComponent: 8, bytesPerRow: 0,
                        space: srgb, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.translateBy(x: 0, y: CGFloat(height * scale))
    ctx.scaleBy(x: CGFloat(scale), y: -CGFloat(scale))
    draw(ctx)

    // Rows of a bitmap context run top-down, as Finder's coordinates do.
    let data = ctx.data!.assumingMemoryBound(to: UInt8.self)
    let s = CGFloat(scale)
    let bg = [UInt8((paper >> 16) & 0xff), UInt8((paper >> 8) & 0xff), UInt8(paper & 0xff), 255]
    for py in 0..<ctx.height {
        for px in 0..<ctx.width {
            let p = CGPoint(x: (CGFloat(px) + 0.5) / s, y: (CGFloat(py) + 0.5) / s)
            let near = iconCentres.contains { hypot($0.x - p.x, $0.y - p.y) < clearRadius }
            guard near || labelRows.contains(p.y) else { continue }
            let o = py * ctx.bytesPerRow + px * 4
            if (0..<4).contains(where: { data[o + $0] != bg[$0] }) {
                FileHandle.standardError.write("make-dmg-background.swift: art at (\(p.x), \(p.y)) pt, under an icon or its label\n".data(using: .utf8)!)
                exit(1)
            }
        }
    }
    return ctx.makeImage()!
}

func writePNG(_ image: CGImage, dpi: Int, to url: URL) {
    guard let dest = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil) else {
        fatalError("cannot write \(url.path)")
    }
    let props = [kCGImagePropertyDPIWidth: dpi, kCGImagePropertyDPIHeight: dpi] as CFDictionary
    CGImageDestinationAddImage(dest, image, props)
    guard CGImageDestinationFinalize(dest) else { fatalError("cannot write \(url.path)") }
}

try FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)
let png1x = outDir.appendingPathComponent("background.png")
let png2x = outDir.appendingPathComponent("background@2x.png")
let tiff = outDir.appendingPathComponent("background.tiff")
writePNG(render(scale: 1), dpi: 72, to: png1x)
writePNG(render(scale: 2), dpi: 144, to: png2x)

let tiffutil = Process()
tiffutil.executableURL = URL(fileURLWithPath: "/usr/bin/tiffutil")
tiffutil.arguments = ["-cathidpicheck", png1x.path, png2x.path, "-out", tiff.path]
// tiffutil reports success on stderr; keep it for failures only.
let tiffutilOutput = Pipe()
tiffutil.standardOutput = tiffutilOutput
tiffutil.standardError = tiffutilOutput
try tiffutil.run()
let tiffutilLog = tiffutilOutput.fileHandleForReading.readDataToEndOfFile()
tiffutil.waitUntilExit()
guard tiffutil.terminationStatus == 0 else {
    FileHandle.standardError.write(tiffutilLog)
    FileHandle.standardError.write("make-dmg-background.swift: tiffutil failed\n".data(using: .utf8)!)
    exit(1)
}
print(tiff.path)
