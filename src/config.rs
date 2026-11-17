use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{auth, util};

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub username: String,
    pub password_hash: String,
}

fn path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ezcontrol.exe"));
    exe.with_file_name("ezcontrol.json")
}

impl Config {
    pub fn load_or_create() -> Config {
        let p = path();
        if let Ok(text) = std::fs::read_to_string(&p) {
            match serde_json::from_str::<Config>(&text) {
                Ok(c) => return c,
                Err(e) => {
                    eprintln!("配置文件 {} 解析失败: {e}", p.display());
                    std::process::exit(1);
                }
            }
        }
        let pw = util::random_password(14);
        let cfg = Config {
            bind: "127.0.0.1".into(),
            port: 8090,
            username: "admin".into(),
            password_hash: auth::hash_password(&pw),
        };
        cfg.save();
        println!("==============================================");
        println!(" 首次运行，已生成配置: {}", p.display());
        println!(" 用户名: admin");
        println!(" 初始密码: {pw}");
        println!(" 请妥善保存，可用 --set-password 修改");
        println!("==============================================");
        cfg
    }

    pub fn save(&self) {
        let p = path();
        let text = serde_json::to_string_pretty(self).unwrap();
        if let Err(e) = std::fs::write(&p, text) {
            eprintln!("写入配置 {} 失败: {e}", p.display());
        }
    }
}
