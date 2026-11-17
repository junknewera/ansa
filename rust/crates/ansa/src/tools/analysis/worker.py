"""Fixed monthly CSV aggregate. Invoked only by the supervised analysis tool."""
import csv
from datetime import date
from decimal import Decimal, localcontext
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import sys


def main():
    root = Path.cwd()
    os.umask(0o077)
    for kind, cap in [(resource.RLIMIT_AS, 1024 * 1024 * 1024),
                      (resource.RLIMIT_FSIZE, 16 * 1024 * 1024),
                      (resource.RLIMIT_NOFILE, 64), (resource.RLIMIT_CPU, 45)]:
        soft, hard = resource.getrlimit(kind)
        resource.setrlimit(kind, (min(soft, cap) if soft >= 0 else cap,
                                 min(hard, cap) if hard >= 0 else cap))
    os.environ["HOME"] = str(root)
    os.environ["MPLCONFIGDIR"] = str(root / "mpl-cache")
    os.environ["MPLBACKEND"] = "Agg"
    os.environ.pop("MATPLOTLIBRC", None)
    os.environ["OPENBLAS_NUM_THREADS"] = "1"
    os.environ["OMP_NUM_THREADS"] = "1"
    parameters = json.loads((root / "parameters.json").read_text())
    date_column, value_column = parameters["date_column"], parameters["value_column"]
    groups = {}
    rows = missing = 0
    csv.field_size_limit(4096)
    with (root / "input.csv").open(newline="", encoding="utf-8") as source:
        reader = csv.DictReader(source, strict=True)
        fields = reader.fieldnames
        if not fields or len(fields) > 64 or len(set(fields)) != len(fields) or any(not field for field in fields):
            raise ValueError("csv_header")
        if date_column not in fields or value_column not in fields:
            raise ValueError("csv_column")
        with localcontext() as context:
            context.prec = 40
            for row in reader:
                rows += 1
                if rows > 100000 or None in row or any(value is None for value in row.values()):
                    raise ValueError("csv_shape")
                day, amount = row[date_column], row[value_column]
                if not day or not amount:
                    missing += 1
                    continue
                if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", day, flags=re.ASCII):
                    raise ValueError("csv_date")
                date.fromisoformat(day)
                # Decimal arithmetic stays exact inside this bound, including
                # 100000 rows. No inferred currency or floating-point sums.
                if not re.fullmatch(r"-?\d{1,18}(?:\.\d{1,6})?", amount, flags=re.ASCII):
                    raise ValueError("csv_number")
                month = day[:7]
                groups[month] = groups.get(month, Decimal(0)) + Decimal(amount)
                if len(groups) > 1200:
                    raise ValueError("csv_groups")
            if not groups:
                raise ValueError("csv_no_values")
            total = sum(groups.values(), Decimal(0))
    ordered = sorted(groups.items())
    with (root / "table.csv").open("x", newline="", encoding="utf-8") as table:
        writer = csv.writer(table)
        writer.writerow(["month", "total"])
        writer.writerows((month, format(value, "f")) for month, value in ordered)
    import matplotlib
    matplotlib.use("Agg", force=True)
    from matplotlib import pyplot as plt
    plt.rcdefaults()
    plt.rcParams.update({"text.usetex": False, "text.parse_math": False})
    figure, axis = plt.subplots(figsize=(10, 5), dpi=100)
    axis.plot([month for month, _ in ordered], [float(value) for _, value in ordered], marker="o", color="#4e9b75")
    stride = max(1, (len(ordered) + 11) // 12)
    axis.set_xticks(range(0, len(ordered), stride), [month for month, _ in ordered][::stride], rotation=35, ha="right")
    axis.set_title("Monthly total")
    axis.set_xlabel("Month")
    axis.set_ylabel("Total")
    axis.grid(alpha=.2)
    figure.tight_layout()
    figure.savefig(root / "chart.png", format="png", dpi=100, bbox_inches=None)
    plt.close(figure)
    lines = ["# Monthly totals", "", f"Rows: {rows}. Missing date/value: {missing}.", "",
             f"Total: {format(total, 'f')}", "", "| Month | Total |", "| --- | ---: |"]
    lines.extend(f"| {month} | {format(value, 'f')} |" for month, value in ordered)
    lines.extend(["", "![Monthly totals](chart.png)", "", "[CSV table](table.csv)", ""])
    (root / "report.md").write_text("\n".join(lines), encoding="utf-8")
    from PIL import Image
    Image.MAX_IMAGE_PIXELS = 8_000_000
    with Image.open(root / "chart.png") as image:
        if image.format != "PNG" or image.n_frames != 1 or image.width * image.height > 8_000_000:
            raise ValueError("png_contract")
        image.verify()
    with Image.open(root / "chart.png") as image:
        image.load()
    summary = dict(rows=rows, missing=missing, months=len(groups), total=format(total, "f"))
    (root / "summary.json").write_text(json.dumps(summary), encoding="utf-8")
    outputs = {}
    for name in ["report.md", "table.csv", "chart.png"]:
        size = (root / name).stat().st_size
        if not 0 < size <= 16 * 1024 * 1024:
            raise ValueError("output_size")
        contents = (root / name).read_bytes()
        outputs[name] = dict(size_bytes=size, sha256=hashlib.sha256(contents).hexdigest())
    inputs = {name: hashlib.sha256((root / name).read_bytes()).hexdigest()
              for name in ["input.csv", "worker.py", "parameters.json"]}
    print(json.dumps(dict(summary=summary, outputs=outputs, inputs=inputs)))


if __name__ == "__main__":
    try:
        main()
    except (ValueError, csv.Error, UnicodeError) as error:
        # Dataset contents and field values are never echoed by the worker.
        print(json.dumps({"error": "invalid_dataset", "category": type(error).__name__}))
        sys.exit(2)
