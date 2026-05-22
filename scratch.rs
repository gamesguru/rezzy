fn main() {
    let s = "!00-m-room-create";
    let id = ruma_common::RoomId::parse(s);
    println!("id: {:?}", id);
}
