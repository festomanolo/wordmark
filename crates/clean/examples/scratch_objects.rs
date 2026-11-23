                ppm.extend_from_slice(&px[..3]);
            }
            std::fs::write(format!("{out}.p{}.ppm", p + 1), ppm).unwrap();
        }
    }
}
