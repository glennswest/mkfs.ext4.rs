fn main() {
    let uuid: [u8;16] = [0x12,0x34,0x56,0x78,0x12,0x34,0x56,0x78,0x9a,0xbc,0x12,0x34,0x56,0x78,0x9a,0xbc];
    println!("golden s_checksum_seed = 0xf053b497");
    println!("append(!0, uuid)   = {:#010x}", crc32c::crc32c_append(!0, &uuid));
    println!("!append(!0, uuid)  = {:#010x}", !crc32c::crc32c_append(!0, &uuid));
    println!("append(0, uuid)    = {:#010x}", crc32c::crc32c_append(0, &uuid));
    println!("!append(0, uuid)   = {:#010x}", !crc32c::crc32c_append(0, &uuid));
}
