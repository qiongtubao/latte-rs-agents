

                                                [1] <头像> 我: 查看代码分析一下把数据结构 A 从list 改成环形数组
[2]manager: @programmer 查找数据A的定义和实现
[3]manager: @architect 分析数据A数据结构使用的上下文

引用[2]
<头像> programmger: 查到定义和实现 
```
class A {
    list* l; 
};
```

引用[3]
<头像> architect: 数据A 使用的上下文
```
A* a;
//添加数据
a->add(new object()); 
```

引用[1]
<头像> manager: 方案1 修改代码只要改A里面的list 换成array
方案2 xxxx


===============================================================
[1] 外部不显示的  只是为了后面的应用加的标识
