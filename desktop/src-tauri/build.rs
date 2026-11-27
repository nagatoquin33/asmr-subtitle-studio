fn main() {
    // 前端是编译期内嵌的静态资源（frontend/）：显式声明依赖，改 index.html / app.js /
    // style.css / ux.css 也会触发重新编译，避免「改了前端但 exe 里还是旧界面」。
    println!("cargo:rerun-if-changed=../../frontend");
    println!("cargo:rerun-if-changed=tauri.conf.json");
    tauri_build::build()
}
