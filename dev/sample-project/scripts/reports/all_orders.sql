-- 1M rows: fetched on demand as you scroll; export streams them all.
select * from shop.orders order by id;
