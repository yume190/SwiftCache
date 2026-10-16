public struct Rect: Sendable {
    public let width: Double
    public let height: Double

    public init(width: Double, height: Double) {
        self.width = width
        self.height = height
    }

    public var area: Double { width * height }
    public var perimeter: Double { 2 * (width + height) }
}

public func describe(_ rects: [Rect]) -> String {
    rects
        .map { "\($0.width)x\($0.height) area=\($0.area) perimeter=\($0.perimeter)" }
        .joined(separator: "\n")
}
