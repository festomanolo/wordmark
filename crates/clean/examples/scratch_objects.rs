        for o in &again.objects {
            println!("after {:6} {:7} {:>4.0}% {:?}", o.id, o.kind.name(), o.score * 100.0, o.text);
        }
        for p in 0..again.pages.len() {
            let pic = wordcraft_clean::objects::render_page(&pdf, None, p, 900).unwrap();
            println!("page {} rendered {}x{}", p + 1, pic.width, pic.height);
            let mut ppm = format!("P6 {} {} 255\n", pic.width, pic.height).into_bytes();
            for px in pic.rgba.chunks(4) {
                ppm.extend_from_slice(&px[..3]);
            }
            std::fs::write(format!("{out}.p{}.ppm", p + 1), ppm).unwrap();
        }
    }
}
