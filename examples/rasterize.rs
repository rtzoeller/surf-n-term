use env_logger::Env;
use rasterize::{surf_to_png, timeit, Align, BBox, FillRule, Path, Point, Scalar, Transform};
use std::{
    env, fmt,
    fs::File,
    io::{BufWriter, Read},
};

type Error = Box<dyn std::error::Error>;

#[derive(Debug)]
struct ArgsError(String);

impl ArgsError {
    fn new(err: impl Into<String>) -> Self {
        Self(err.into())
    }
}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::error::Error for ArgsError {}

struct Args {
    input_file: String,
    output_file: String,
    width: Option<usize>,
}

fn parse_args() -> Result<Args, Error> {
    let mut result = Args {
        input_file: String::new(),
        output_file: String::new(),
        width: None,
    };
    let mut postional = 0;
    let mut args = env::args();
    let _cmd = args.next().unwrap();
    while let Some(arg) = args.next() {
        match arg.as_ref() {
            "-w" => {
                let width = args
                    .next()
                    .ok_or_else(|| ArgsError::new("-w requires argument"))?;
                result.width = Some(width.parse()?);
            }
            _ => {
                postional += 1;
                match postional {
                    1 => result.input_file = arg,
                    2 => result.output_file = arg,
                    _ => return Err(ArgsError::new("unexpected positional argment").into()),
                }
            }
        }
    }
    if postional < 2 {
        return Err(ArgsError::new("Usage: rasterize [-w <width>] <file.path> <out.png>").into());
    }
    Ok(result)
}

fn path_load(path: String) -> Result<Path, Error> {
    let mut contents = String::new();
    if path != "-" {
        let mut file = File::open(path)?;
        file.read_to_string(&mut contents)?;
    } else {
        std::io::stdin().read_to_string(&mut contents)?;
    }
    Ok(timeit("[parse]", || contents.parse())?)
}

fn main() -> Result<(), Error> {
    env_logger::from_env(Env::default().default_filter_or("debug")).init();
    let args = parse_args()?;

    let path = path_load(args.input_file)?;
    let tr = match args.width {
        Some(width) if width > 2 => {
            let src_bbox = path
                .bbox(Transform::default())
                .ok_or_else(|| ArgsError::new("path is empty"))?;
            let width = width as Scalar;
            let height = src_bbox.height() * width / src_bbox.width();
            let dst_bbox = BBox::new(Point::new(1.0, 1.0), Point::new(width - 1.0, height - 1.0));
            Transform::fit(src_bbox, dst_bbox, Align::Mid)
        }
        _ => Transform::default(),
    };
    let mask = timeit("[rasterize]", || path.rasterize(tr, FillRule::NonZero));

    if args.output_file != "-" {
        let mut image = BufWriter::new(File::create(args.output_file)?);
        timeit("[save:png]", || surf_to_png(&mask, &mut image))?;
    } else {
        timeit("[save:png]", || surf_to_png(&mask, std::io::stdout()))?;
    }

    Ok(())
}
