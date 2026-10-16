import DemoLib

let rects = [
    Rect(width: 3, height: 4),
    Rect(width: 10, height: 2.5),
]

print(describe(rects))
print("mean area:", Statistics.mean(rects.map(\.area)))
print("variance :", Statistics.variance(rects.map(\.area)))
