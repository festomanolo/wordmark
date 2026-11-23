            println!("page {} rendered {}x{}", p + 1, pic.width, pic.height);
            let mut ppm = format!("P6 {} {} 255\n", pic.width, pic.height).into_bytes();
            for px in pic.rgba.chunks(4) {
                ppm.extend_from_slice(&px[..3]);
            }
            std::fs::write(format!("{out}.p{}.ppm", p + 1), ppm).unwrap();
        }
    }
}
