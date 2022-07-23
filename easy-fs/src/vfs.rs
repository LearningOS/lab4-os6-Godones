use super::{
    BlockDevice,
    DiskInode,
    DiskInodeType,
    DirEntry,
    EasyFileSystem,
    DIRENT_SZ,
    get_block_cache,
    block_cache_sync_all,
};
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};

/// Virtual filesystem layer over easy-fs
pub struct Inode {
    block_id: usize,
    block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
}

impl Inode {
    /// Create a vfs inode
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
        }
    }
    /// Call a function over a disk inode to read it
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(
            self.block_id,
            Arc::clone(&self.block_device)
        ).lock().read(self.block_offset, f)
    }
    /// Call a function over a disk inode to modify it
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(
            self.block_id,
            Arc::clone(&self.block_device)
        ).lock().modify(self.block_offset, f)
    }
    /// Find inode under a disk inode by name
    fn find_inode_id(
        &self,
        name: &str,
        disk_inode: &DiskInode,
    ) -> Option<u32> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(
                    DIRENT_SZ * i,
                    dirent.as_bytes_mut(),
                    &self.block_device,
                ),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some(dirent.inode_number() as u32);
            }
        }
        None
    }
    /// Find inode under current inode by name
    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            self.find_inode_id(name, disk_inode)
            .map(|inode_id| {
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                ))
            })
        })
    }
    /// Increase the size of a disk inode
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size < disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }
    /// Create inode under current inode by name
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let mut fs = self.fs.lock();
        if self.modify_disk_inode(|root_inode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        }).is_some() {
            return None;
        }
        // create a new file
        // alloc a inode with an indirect block
        let new_inode_id = fs.alloc_inode();
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) 
            = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(
            new_inode_block_id as usize,
            Arc::clone(&self.block_device)
        ).lock().modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
            new_inode.initialize(DiskInodeType::File);
        });
        self.modify_disk_inode(|root_inode| {
            // append file in the dirent
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            // increase size
            self.increase_size(new_size as u32, root_inode, &mut fs);
            // write dirent
            let dirent = DirEntry::new(name, new_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });

        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        block_cache_sync_all();
        // return inode
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
        )))
        // release efs lock automatically by compiler
    }
    /// List inodes under current inode
    pub fn ls(&self) -> Vec<String> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut v: Vec<String> = Vec::new();
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(
                        i * DIRENT_SZ,
                        dirent.as_bytes_mut(),
                        &self.block_device,
                    ),
                    DIRENT_SZ,
                );
                v.push(String::from(dirent.name()));
            }
            v
        })
    }
    /// Read data from current inode
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            disk_inode.read_at(offset, buf, &self.block_device)
        })
    }
    /// Write data to current inode
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        let size = self.modify_disk_inode(|disk_inode| {
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        });
        block_cache_sync_all();
        size
    }
    /// Clear the data in current inode
    pub fn clear(&self) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
        block_cache_sync_all();
    }
}

impl Inode {
    /// 查看文件inode编号
    pub fn get_disk_inode(&self)->usize{
        let fs = self.fs.lock();
        fs.get_disk_inode(self.block_id as u32,self.block_offset) as usize
    }
    pub fn find_inode(&self,name:&str)->Option<Arc<Inode>>{
        //根据名称找到文件索引节点号
        let fs = self.fs.lock();//尝试获得文件系统的互斥锁
        self.read_disk_inode(|disk_inode|{
            self.find_inode_id(name,disk_inode).map(|inode_id|{
                let(block_id,block_offset) = fs.get_disk_inode_pos(inode_id);
                // println!("the inode id: {}, The block_id :{}, block_offset :{}",inode_id,block_id,block_offset);
                Arc::new(
                    Inode::new(
                        block_id,
                        block_offset,
                        self.fs.clone(),
                        self.block_device.clone(),
                    )
                )
            })
        })
    }
    pub fn create_nlink(&self,newname:&str,oldname:&str)->Option<Arc<Inode>>{
        //创建一个硬链接文件
        if self.modify_disk_inode(|root_node:&mut DiskInode|{
            //查找是否已经存在此节点
            assert!(root_node.is_dir(),"The root node is not directory");
            self.find_inode_id(newname,root_node)//在根目录下查找
        }).is_some(){
            return None//存在文件
        }
        //新建一个文件
        let old_inode = self.find_inode(oldname).unwrap();
        let (inode_block_id,inode_block_offset) = (old_inode.block_id,old_inode.block_offset);
        self.modify_disk_inode(|root_inode|{
            //在根目录下添加
            let file_num = root_inode.size as usize/DIRENT_SZ;
            let new_size = (file_num+1)*DIRENT_SZ;//新的目录大小

            let new_entry = DirEntry::new(newname, old_inode.get_disk_inode()as u32);
            let mut fs = self.fs.lock();
            self.increase_size(new_size as u32,root_inode,&mut fs);

            let _number = root_inode.write_at(
                file_num*DIRENT_SZ as usize,
                new_entry.as_bytes(),
                &self.block_device,
            );//写入目录项
        });
        let new_inode = Inode::new(
            inode_block_id as u32,
            inode_block_offset,
            self.fs.clone(),
            self.block_device.clone()
        );
        new_inode.add_disk_nlink();//添加硬链接
        Some(Arc::new(new_inode))
    }
    pub fn delete_nlink(&self,path:&str)->isize{
        //只需要找到文件inode并且将其换成一个空白文件即可
        let inode = self.find_inode(path).unwrap();//找到文件inode
        //需要从目录下删除文件并减少文件的硬链接计数
        inode.sub_disk_nlink();
        if inode.get_disk_nlink()==0 {
            inode.clear();
            self.delete_file(path);
        }
        0
    }

    pub fn get_disk_nlink(&self)->u32{
        self.read_disk_inode(|disknode|{
            disknode.nlink
        })
    }

    pub fn add_disk_nlink(&self){
        self.modify_disk_inode(|disknode|{
            disknode.nlink +=1;
        })
    }
    pub fn sub_disk_nlink(&self){
        self.modify_disk_inode(|disknode|{
            disknode.nlink -=1;
        })
    }
    ///查看文件大小
    pub fn get_file_size(&self)->usize{
        self.read_disk_inode(|disk_node|{
            disk_node.size as usize
        })
    }
    ///查看文件类型
    pub fn get_disk_type(&self)->u32{
        self.read_disk_inode(|disknode|{
            if disknode.is_dir(){
                0o040000
            }
            else {
                0o100000
            }
        })
    }
    pub fn delete_file(&self,path:&str)->isize{
        //删除文件
        self.modify_disk_inode(|root_inode| {
            // append file in the dirent
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            // 找到对应的目录项
            for index in 0..file_count {
                let mut entry = DirEntry::empty();
                root_inode.read_at(
                    index * DIRENT_SZ as usize,
                    entry.as_bytes_mut(),
                    &self.block_device,
                );
                if entry.name() == path {
                    // 删除目录项
                    root_inode.write_at(
                        index * DIRENT_SZ as usize,
                        DirEntry::empty().as_bytes(),
                        &self.block_device,
                    );
                }
            }
        });
        0
    }
}