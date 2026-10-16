struct Rect {
    let width: Double
    let height: Double

    var area: Double { width * height }
    var perimeter: Double { 2 * (width + height) }
}

func describe(_ rects: [Rect]) -> String {
    rects
        .map { "\($0.width)x\($0.height) area=\($0.area) perimeter=\($0.perimeter)" }
        .joined(separator: "\n")
}
