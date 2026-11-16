// 生成 SuperControl 应用图标（1024×1024 PNG）：黑色圆角方块 + 白色圆角六边形 + 两块 L 形拼成的黑色十字。
// 用法：swift packaging/icon/draw-icon.swift <输出.png> [--flat]
//   --flat 不留 macOS 图标网格的外边距（Windows 图标用）
import AppKit

let args = CommandLine.arguments
let out = args.count > 1 ? args[1] : "icon.png"
let flat = args.contains("--flat")
let S: CGFloat = 1024
let cs = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: Int(S), height: Int(S), bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
ctx.translateBy(x: 0, y: S); ctx.scaleBy(x: 1, y: -1) // 左上角为原点

func gray(_ v: CGFloat, _ a: CGFloat = 1) -> CGColor { CGColor(srgbRed: v, green: v, blue: v, alpha: a) }

// 1. 底板：macOS 图标网格 824×824；Windows 用满画布
let inset: CGFloat = flat ? 24 : 100
let body = CGRect(x: inset, y: inset, width: S - 2 * inset, height: S - 2 * inset)
do {
    let radius = body.width * 0.225
    let bodyPath = CGPath(roundedRect: body, cornerWidth: radius, cornerHeight: radius, transform: nil)
    if !flat {
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -8), blur: 24, color: gray(0, 0.3))
        ctx.addPath(bodyPath); ctx.setFillColor(gray(0.08)); ctx.fillPath()
        ctx.restoreGState()
    }
    ctx.saveGState()
    ctx.addPath(bodyPath); ctx.clip()
    let grad = CGGradient(colorsSpace: cs, colors: [gray(0.17), gray(0.02)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(grad, start: CGPoint(x: body.midX, y: body.minY), end: CGPoint(x: body.midX, y: body.maxY), options: [])
    ctx.restoreGState()
}

// 2. 白色圆角六边形（左右两端为尖角）
let c = CGPoint(x: body.midX, y: body.midY)
let R = body.width * 0.36
let corner = R * 0.2
var pts: [CGPoint] = []
for i in 0..<6 {
    let a = CGFloat(i) * .pi / 3
    pts.append(CGPoint(x: c.x + R * cos(a), y: c.y + R * sin(a)))
}
let hex = CGMutablePath()
// 圆角：从每条边的中点出发，用 arc(tangent) 绕过顶点
let mid = { (p: CGPoint, q: CGPoint) in CGPoint(x: (p.x + q.x) / 2, y: (p.y + q.y) / 2) }
hex.move(to: mid(pts[5], pts[0]))
for i in 0..<6 {
    hex.addArc(tangent1End: pts[i], tangent2End: mid(pts[i], pts[(i + 1) % 6]), radius: corner)
}
hex.closeSubpath()
ctx.addPath(hex); ctx.setFillColor(gray(1)); ctx.fillPath()

// 3. 十字：两块 L 形（上臂接右臂、下臂接左臂），转角处是大圆弧，中间留出一道 S 形的白缝
let t = R * 0.32          // 臂宽
let L = R * 0.74          // 臂长（从中心算）
let g = t * 0.06          // 中缝的一半
let rho = (g + t / 2) / (1 - 1 / sqrt(2.0))  // 转角圆弧半径：刚好让两块在中心错开 2g
let k = rho - t / 2       // 圆弧圆心到中心的偏移
// 一块：上臂 → 四分之一圆环 → 右臂（圆环外缘半径 rho，内缘 rho - t，圆心在中心右上方）
let kappa: CGFloat = 0.5523 // 用三次贝塞尔近似四分之一圆
func piece() -> CGPath {
    let ri = rho - t
    let p = CGMutablePath()
    p.move(to: CGPoint(x: -t / 2, y: -L))
    p.addLine(to: CGPoint(x: -t / 2, y: -k))
    p.addCurve(to: CGPoint(x: k, y: t / 2), control1: CGPoint(x: -t / 2, y: -k + kappa * rho), control2: CGPoint(x: k - kappa * rho, y: t / 2))
    p.addLine(to: CGPoint(x: L, y: t / 2))
    p.addLine(to: CGPoint(x: L, y: -t / 2))
    p.addLine(to: CGPoint(x: k, y: -t / 2))
    p.addCurve(to: CGPoint(x: t / 2, y: -k), control1: CGPoint(x: k - kappa * ri, y: -t / 2), control2: CGPoint(x: t / 2, y: -k + kappa * ri))
    p.addLine(to: CGPoint(x: t / 2, y: -L))
    p.closeSubpath()
    return p
}
ctx.setFillColor(gray(0.02))
for i in 0..<2 {
    ctx.saveGState()
    ctx.translateBy(x: c.x, y: c.y)
    ctx.rotate(by: CGFloat(i) * .pi)
    ctx.addPath(piece()); ctx.fillPath()
    ctx.restoreGState()
}

let img = ctx.makeImage()!
try! NSBitmapImageRep(cgImage: img).representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
